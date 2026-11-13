//! Métricas de conexão e utilitários de identificação dos espectadores.
//! Contém a estrutura ViewerStats, sanitização de métricas recebidas do navegador,
//! mascaramento de IP para privacidade e geração de identificadores de espectador.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewerStats {
    pub rtt_ms: u32,
    pub loss_pct: f32,
    pub fps: u32,
    pub kbps: u32,
    pub path: String,
    pub reconnects: u32,
    pub last_reason: String,
}

impl Default for ViewerStats {
    fn default() -> Self {
        Self {
            rtt_ms: 0,
            loss_pct: 0.0,
            fps: 0,
            kbps: 0,
            path: "direct".to_string(),
            reconnects: 0,
            last_reason: String::new(),
        }
    }
}

pub const ALLOWED_LAST_REASONS: &[&str] = &[
    "travou",
    "ice-failed",
    "ice-closed",
    "ice-desconectado",
    "ws-fechado",
    "timeout",
    "nenhum",
    "iniciou",
];

pub fn generate_viewer_id() -> String {
    let bytes: [u8; 3] = rand::random();
    format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

pub fn mask_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.x", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}", s[0], s[1], s[2])
        }
    }
}

pub fn client_ip(headers: &HeaderMap, socket_addr: SocketAddr) -> (IpAddr, String) {
    if let Some(cf_ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        let masked = mask_ip(cf_ip);
        (cf_ip, masked)
    } else {
        let ip = socket_addr.ip();
        let masked = mask_ip(ip);
        (ip, masked)
    }
}

pub fn sanitize_stats(raw: &Value) -> Option<ViewerStats> {
    let rtt_ms = raw
        .get("rtt_ms")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 60_000.0) as u32)
        .unwrap_or(0);

    let loss_pct = raw
        .get("loss_pct")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 100.0) as f32)
        .unwrap_or(0.0);

    let fps = raw
        .get("fps")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 240.0) as u32)
        .unwrap_or(0);

    let kbps = raw
        .get("kbps")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 100_000.0) as u32)
        .unwrap_or(0);

    let reconnects = raw
        .get("reconnects")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 10_000.0) as u32)
        .unwrap_or(0);

    let path_str = raw.get("path").and_then(|v| v.as_str()).unwrap_or("direct");
    let path = if path_str.eq_ignore_ascii_case("relay") {
        "relay".to_string()
    } else {
        "direct".to_string()
    };

    let reason_raw = raw.get("last_reason").and_then(|v| v.as_str()).unwrap_or("");
    let truncated: String = reason_raw.chars().take(32).collect();
    let last_reason = if ALLOWED_LAST_REASONS.iter().any(|&r| r == truncated) {
        truncated
    } else if truncated.is_empty() {
        String::new()
    } else {
        "outro".to_string()
    };

    Some(ViewerStats {
        rtt_ms,
        loss_pct,
        fps,
        kbps,
        path,
        reconnects,
        last_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use axum::http::HeaderValue;
    use serde_json::json;

    #[test]
    fn mask_ip_handles_ipv4_and_ipv6() {
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        assert_eq!(mask_ip(v4), "192.168.1.x");

        let v4_loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(mask_ip(v4_loopback), "127.0.0.x");

        let v6: IpAddr = "2001:db8:85a3:0:0:8a2e:370:7334".parse().unwrap();
        assert_eq!(mask_ip(v6), "2001:db8:85a3");

        let v6_loopback: IpAddr = "::1".parse().unwrap();
        assert_eq!(mask_ip(v6_loopback), "0:0:0");
    }

    #[test]
    fn client_ip_prefers_cf_connecting_ip_header() {
        let socket = SocketAddr::from(([127, 0, 0, 1], 12345));

        // Sem cabeçalho: usa o endereço do socket
        let headers = HeaderMap::new();
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(masked, "127.0.0.x");

        // Com cf-connecting-ip IPv4
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.195"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, "203.0.113.195".parse::<IpAddr>().unwrap());
        assert_eq!(masked, "203.0.113.x");

        // Com cf-connecting-ip IPv6
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("2804:14d:5c73:89b0::1"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, "2804:14d:5c73:89b0::1".parse::<IpAddr>().unwrap());
        assert_eq!(masked, "2804:14d:5c73");

        // Com valor inválido no cabeçalho: fallback para o socket
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("invalido"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(masked, "127.0.0.x");
    }

    #[test]
    fn viewer_id_is_6_hex_chars() {
        let id1 = generate_viewer_id();
        let id2 = generate_viewer_id();
        assert_eq!(id1.len(), 6);
        assert_eq!(id2.len(), 6);
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id2.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sanitize_stats_clamps_numbers_and_restricts_strings() {
        // Objeto válido normal
        let raw = json!({
            "rtt_ms": 45,
            "loss_pct": 2.5,
            "fps": 60,
            "kbps": 5000,
            "path": "relay",
            "reconnects": 1,
            "last_reason": "travou"
        });
        let stats = sanitize_stats(&raw).expect("stats válido");
        assert_eq!(stats.rtt_ms, 45);
        assert_eq!(stats.loss_pct, 2.5);
        assert_eq!(stats.fps, 60);
        assert_eq!(stats.kbps, 5000);
        assert_eq!(stats.path, "relay");
        assert_eq!(stats.reconnects, 1);
        assert_eq!(stats.last_reason, "travou");

        // Clamping de limites superiores e inferiores
        let raw_overflow = json!({
            "rtt_ms": 999_999,
            "loss_pct": 150.0,
            "fps": 500,
            "kbps": 999_999,
            "path": "invalid_path",
            "reconnects": 50_000,
            "last_reason": "motivo_desconhecido"
        });
        let stats = sanitize_stats(&raw_overflow).expect("clamped");
        assert_eq!(stats.rtt_ms, 60_000);
        assert_eq!(stats.loss_pct, 100.0);
        assert_eq!(stats.fps, 240);
        assert_eq!(stats.kbps, 100_000);
        assert_eq!(stats.path, "direct");
        assert_eq!(stats.reconnects, 10_000);
        assert_eq!(stats.last_reason, "outro");

        // Clamping negativo
        let raw_underflow = json!({
            "rtt_ms": -10,
            "loss_pct": -20.5,
            "fps": -5,
            "kbps": -100,
            "reconnects": -2,
        });
        let stats = sanitize_stats(&raw_underflow).expect("clamped zeros");
        assert_eq!(stats.rtt_ms, 0);
        assert_eq!(stats.loss_pct, 0.0);
        assert_eq!(stats.fps, 0);
        assert_eq!(stats.kbps, 0);
        assert_eq!(stats.reconnects, 0);

        // String longa deve ser truncada em 32 chars
        let long_reason = "a".repeat(100);
        let raw_long = json!({
            "last_reason": long_reason
        });
        let stats = sanitize_stats(&raw_long).expect("truncated");
        assert_eq!(stats.last_reason, "outro");
    }
}
