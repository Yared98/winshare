//! Registro de saúde e métricas dos espectadores conectados.
//! Armazena os registros individuais (tempo de conexão e estatísticas WebRTC)
//! e fornece a listagem ordenada para o painel de controle.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::server::api::ViewerStatusItem;
use crate::server::viewer_stats::ViewerStats;

pub struct ViewerRecord {
    pub id: String,
    pub connected_at: Instant,
    pub stats: ViewerStats,
}

#[derive(Default)]
pub struct ViewerRegistry {
    records: Mutex<HashMap<String, ViewerRecord>>,
}

impl ViewerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: String) {
        let mut map = self.records.lock().expect("ViewerRegistry poisoned");
        map.insert(
            id.clone(),
            ViewerRecord {
                id,
                connected_at: Instant::now(),
                stats: ViewerStats::default(),
            },
        );
    }

    pub fn update(&self, id: &str, stats: ViewerStats) {
        let mut map = self.records.lock().expect("ViewerRegistry poisoned");
        if let Some(record) = map.get_mut(id) {
            record.stats = stats;
        }
    }

    pub fn remove(&self, id: &str) {
        let mut map = self.records.lock().expect("ViewerRegistry poisoned");
        map.remove(id);
    }

    pub fn viewer_list(&self) -> Vec<ViewerStatusItem> {
        let map = self.records.lock().expect("ViewerRegistry poisoned");
        let mut list: Vec<ViewerStatusItem> = map
            .values()
            .map(|r| ViewerStatusItem {
                id: r.id.clone(),
                connected_secs: r.connected_at.elapsed().as_secs(),
                path: r.stats.path.clone(),
                rtt_ms: r.stats.rtt_ms,
                loss_pct: r.stats.loss_pct,
                fps: r.stats.fps,
                kbps: r.stats.kbps,
                reconnects: r.stats.reconnects,
                last_reason: r.stats.last_reason.clone(),
            })
            .collect();
        list.sort_by(|a, b| b.connected_secs.cmp(&a.connected_secs).then_with(|| a.id.cmp(&b.id)));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_and_removes() {
        let registry = ViewerRegistry::new();
        assert!(registry.viewer_list().is_empty());

        registry.register("a1b2c3".to_string());
        let list = registry.viewer_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a1b2c3");
        assert_eq!(list[0].path, "direct");
        assert_eq!(list[0].rtt_ms, 0);

        registry.update(
            "a1b2c3",
            ViewerStats {
                rtt_ms: 35,
                loss_pct: 1.2,
                fps: 59,
                kbps: 4500,
                path: "relay".to_string(),
                reconnects: 2,
                last_reason: "travou".to_string(),
            },
        );

        let list = registry.viewer_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].rtt_ms, 35);
        assert_eq!(list[0].loss_pct, 1.2);
        assert_eq!(list[0].fps, 59);
        assert_eq!(list[0].kbps, 4500);
        assert_eq!(list[0].path, "relay");
        assert_eq!(list[0].reconnects, 2);
        assert_eq!(list[0].last_reason, "travou");

        registry.remove("a1b2c3");
        assert!(registry.viewer_list().is_empty());
    }
}
