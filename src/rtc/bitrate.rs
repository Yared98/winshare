//! Controle de taxa adaptativa do vídeo com base nas perdas reportadas por RTCP (Receiver Reports).
//!
//! Struct pura e testável: guarda a última perda de cada peer e decide pela pior perda.
//! - Perda > 10%: alvo * 0.85
//! - Perda < 2% por 4 s seguidos: alvo * 1.08
//! - Limites [video_min_bitrate_kbps, video_bitrate_kbps]
//! - Mudança aplicada no encoder no máximo a cada 1 s
//! - Sem peers: volta ao máximo

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Estado da transmissão para monitoramento e painel de controle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamHealth {
    pub target_kbps: u32,
    pub worst_loss_pct: f64,
}

#[derive(Debug, Clone)]
pub struct BitrateController {
    min_bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    current_target_kbps: u32,
    /// Guarda a última fração de perda (em porcentagem: 0.0 a 100.0) de cada peer.
    peer_loss: HashMap<u64, f64>,
    /// Início do período contínuo em que a pior perda esteve abaixo de 2%.
    low_loss_start: Option<Instant>,
    /// Último instante em que a taxa foi alterada.
    last_change: Option<Instant>,
}

impl BitrateController {
    /// Cria o controlador de taxa com os limites configurados. A taxa inicial é o teto máximo.
    pub fn new(min_bitrate_kbps: u32, max_bitrate_kbps: u32) -> Self {
        assert!(
            min_bitrate_kbps <= max_bitrate_kbps,
            "min_bitrate_kbps ({min_bitrate_kbps}) não pode ser maior que max_bitrate_kbps ({max_bitrate_kbps})"
        );
        Self {
            min_bitrate_kbps,
            max_bitrate_kbps,
            current_target_kbps: max_bitrate_kbps,
            peer_loss: HashMap::new(),
            low_loss_start: None,
            last_change: None,
        }
    }

    pub fn min_bitrate_kbps(&self) -> u32 {
        self.min_bitrate_kbps
    }

    pub fn max_bitrate_kbps(&self) -> u32 {
        self.max_bitrate_kbps
    }

    pub fn target_kbps(&self) -> u32 {
        self.current_target_kbps
    }

    /// Pior perda (maior porcentagem de perda) entre os peers conectados no momento.
    pub fn worst_loss_pct(&self) -> f64 {
        if self.peer_loss.is_empty() {
            0.0
        } else {
            self.peer_loss
                .values()
                .copied()
                .fold(0.0f64, f64::max)
        }
    }

    /// Retorna o estado atual da transmissão (taxa e pior perda).
    pub fn health(&self) -> StreamHealth {
        StreamHealth {
            target_kbps: self.target_kbps(),
            worst_loss_pct: self.worst_loss_pct(),
        }
    }

    /// Registra um novo peer.
    pub fn add_peer(&mut self, peer_id: u64) {
        self.peer_loss.entry(peer_id).or_insert(0.0);
    }

    /// Remove um peer. Ao fechar o peer, ele sai do controle.
    /// Sem peers: volta imediatamente ao máximo.
    pub fn remove_peer(&mut self, peer_id: u64, now: Instant) -> Option<u32> {
        self.peer_loss.remove(&peer_id);
        if self.peer_loss.is_empty() {
            self.low_loss_start = None;
            if self.current_target_kbps != self.max_bitrate_kbps {
                let old = self.current_target_kbps;
                self.current_target_kbps = self.max_bitrate_kbps;
                self.last_change = Some(now);
                tracing::info!(
                    old_kbps = old,
                    new_kbps = self.current_target_kbps,
                    "sem peers conectados: taxa de vídeo retornou ao máximo"
                );
                return Some(self.current_target_kbps);
            }
            return None;
        }
        self.evaluate(now)
    }

    /// Atualiza a perda de um peer (em porcentagem 0.0..=100.0) e reavalia a taxa.
    pub fn update_loss(&mut self, peer_id: u64, loss_pct: f64, now: Instant) -> Option<u32> {
        self.peer_loss.insert(peer_id, loss_pct.clamp(0.0, 100.0));
        self.evaluate(now)
    }

    /// Chamado periodicamente ou a cada quadro para avaliar subidas de taxa dependentes de tempo.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        self.evaluate(now)
    }

    fn evaluate(&mut self, now: Instant) -> Option<u32> {
        if self.peer_loss.is_empty() {
            if self.current_target_kbps != self.max_bitrate_kbps {
                let old = self.current_target_kbps;
                self.current_target_kbps = self.max_bitrate_kbps;
                self.last_change = Some(now);
                self.low_loss_start = None;
                tracing::info!(
                    old_kbps = old,
                    new_kbps = self.current_target_kbps,
                    "sem peers conectados: taxa de vídeo retornou ao máximo"
                );
                return Some(self.current_target_kbps);
            }
            return None;
        }

        let worst_loss = self.worst_loss_pct();

        // Mudança aplicada no encoder no máximo a cada 1 s
        let can_change = self
            .last_change
            .map_or(true, |t| now.duration_since(t) >= Duration::from_secs(1));

        if worst_loss > 10.0 {
            // Perda > 10%: alvo * 0.85
            self.low_loss_start = None;
            if can_change {
                let calculated = (self.current_target_kbps as f64 * 0.85).round() as u32;
                let new_target = calculated.clamp(self.min_bitrate_kbps, self.max_bitrate_kbps);
                if new_target != self.current_target_kbps {
                    let old = self.current_target_kbps;
                    self.current_target_kbps = new_target;
                    self.last_change = Some(now);
                    tracing::info!(
                        old_kbps = old,
                        new_kbps = self.current_target_kbps,
                        worst_loss_pct = worst_loss,
                        "taxa de vídeo reduzida por alta perda de pacotes"
                    );
                    return Some(self.current_target_kbps);
                }
            }
        } else if worst_loss < 2.0 {
            // Perda < 2% por 4 s seguidos: alvo * 1.08
            match self.low_loss_start {
                None => {
                    self.low_loss_start = Some(now);
                }
                Some(start) => {
                    if now.duration_since(start) >= Duration::from_secs(4) && can_change {
                        let calculated = (self.current_target_kbps as f64 * 1.08).round() as u32;
                        let new_target = calculated.clamp(self.min_bitrate_kbps, self.max_bitrate_kbps);
                        self.low_loss_start = Some(now); // Reinicia a janela de 4 s
                        if new_target != self.current_target_kbps {
                            let old = self.current_target_kbps;
                            self.current_target_kbps = new_target;
                            self.last_change = Some(now);
                            tracing::info!(
                                old_kbps = old,
                                new_kbps = self.current_target_kbps,
                                worst_loss_pct = worst_loss,
                                "taxa de vídeo aumentada por baixa perda contínua"
                            );
                            return Some(self.current_target_kbps);
                        }
                    }
                }
            }
        } else {
            // Perda entre 2% e 10%: taxa estável; interrompe sequência de baixa perda
            self.low_loss_start = None;
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_with_high_loss() {
        let mut ctrl = BitrateController::new(800, 3000);
        let t0 = Instant::now();
        ctrl.add_peer(1);

        // Perda de 15% (> 10%): alvo deve cair para 3000 * 0.85 = 2550
        let change = ctrl.update_loss(1, 15.0, t0);
        assert_eq!(change, Some(2550));
        assert_eq!(ctrl.target_kbps(), 2550);

        // Nova perda alta antes de 1 s: não muda ainda (limite de 1 s)
        let t_half = t0 + Duration::from_millis(500);
        let change = ctrl.update_loss(1, 15.0, t_half);
        assert_eq!(change, None);
        assert_eq!(ctrl.target_kbps(), 2550);

        // Nova perda alta após 1 s: cai novamente para 2550 * 0.85 = 2168
        let t1 = t0 + Duration::from_millis(1050);
        let change = ctrl.update_loss(1, 15.0, t1);
        assert_eq!(change, Some(2168));
        assert_eq!(ctrl.target_kbps(), 2168);
    }

    #[test]
    fn slow_rise_with_low_loss() {
        let mut ctrl = BitrateController::new(800, 3000);
        let t0 = Instant::now();
        ctrl.add_peer(1);

        // Reduz primeiro para poder subir
        ctrl.update_loss(1, 20.0, t0);
        assert_eq!(ctrl.target_kbps(), 2550);

        // Inicia perda baixa (< 2%) em t1 (1 s depois)
        let t1 = t0 + Duration::from_secs(1);
        let change = ctrl.update_loss(1, 1.0, t1);
        assert_eq!(change, None, "ainda não deu 4 s de perda baixa");

        // Após 2 s de perda baixa: nada ainda
        let t2 = t1 + Duration::from_secs(2);
        let change = ctrl.tick(t2);
        assert_eq!(change, None);

        // Após 3.9 s: nada ainda
        let t3 = t1 + Duration::from_millis(3900);
        let change = ctrl.tick(t3);
        assert_eq!(change, None);

        // Após 4 s seguidos de perda baixa: sobe para 2550 * 1.08 = 2754
        let t4 = t1 + Duration::from_millis(4050);
        let change = ctrl.tick(t4);
        assert_eq!(change, Some(2754));
        assert_eq!(ctrl.target_kbps(), 2754);

        // Se a perda subir temporariamente para 5% (>= 2%), reinicia a contagem
        let t5 = t4 + Duration::from_secs(1);
        ctrl.update_loss(1, 5.0, t5);

        // Volta a 1% em t6
        let t6 = t5 + Duration::from_secs(1);
        ctrl.update_loss(1, 1.0, t6);

        // 3 s depois de t6: não sobe
        let t7 = t6 + Duration::from_secs(3);
        assert_eq!(ctrl.tick(t7), None);

        // 4 s depois de t6: sobe para 2754 * 1.08 = 2974
        let t8 = t6 + Duration::from_secs(4);
        assert_eq!(ctrl.tick(t8), Some(2974));
        assert_eq!(ctrl.target_kbps(), 2974);
    }

    #[test]
    fn limits_enforced() {
        let mut ctrl = BitrateController::new(800, 1000);
        let t0 = Instant::now();
        ctrl.add_peer(1);

        // Queda com perda de 50%: 1000 * 0.85 = 850
        let c1 = ctrl.update_loss(1, 50.0, t0);
        assert_eq!(c1, Some(850));

        // Próxima queda após 1 s: 850 * 0.85 = 723, mas clamp no mínimo 800
        let t1 = t0 + Duration::from_millis(1100);
        let c2 = ctrl.update_loss(1, 50.0, t1);
        assert_eq!(c2, Some(800));
        assert_eq!(ctrl.target_kbps(), 800);

        // Nova queda após 1 s: já está no mínimo 800, não muda
        let t2 = t1 + Duration::from_millis(1100);
        let c3 = ctrl.update_loss(1, 50.0, t2);
        assert_eq!(c3, None);
        assert_eq!(ctrl.target_kbps(), 800);

        // Teste de teto máximo:
        let mut ctrl_max = BitrateController::new(800, 3000);
        ctrl_max.add_peer(1);
        // Em 3000, perda baixa por 4 s não pode ultrapassar 3000
        ctrl_max.update_loss(1, 0.5, t0);
        let t_4s = t0 + Duration::from_secs(4);
        assert_eq!(ctrl_max.tick(t_4s), None);
        assert_eq!(ctrl_max.target_kbps(), 3000);
    }

    #[test]
    fn peer_removed_and_returns_to_max() {
        let mut ctrl = BitrateController::new(800, 3000);
        let t0 = Instant::now();
        ctrl.add_peer(1);
        ctrl.add_peer(2);

        // Peer 1 tem perda alta de 20%, Peer 2 tem perda de 0%
        // A decisão é pela PIOR perda (20%)
        let change = ctrl.update_loss(1, 20.0, t0);
        assert_eq!(change, Some(2550));
        assert_eq!(ctrl.worst_loss_pct(), 20.0);

        // Peer 1 desconecta: pior perda agora é 0.0% (de Peer 2)
        let t1 = t0 + Duration::from_secs(1);
        ctrl.remove_peer(1, t1);
        assert_eq!(ctrl.worst_loss_pct(), 0.0);
        // Não voltou ao máximo ainda porque Peer 2 ainda existe
        assert_eq!(ctrl.target_kbps(), 2550);

        // Peer 2 desconecta: sem peers, volta imediatamente ao máximo (3000)
        let t2 = t1 + Duration::from_millis(500);
        let back_to_max = ctrl.remove_peer(2, t2);
        assert_eq!(back_to_max, Some(3000));
        assert_eq!(ctrl.target_kbps(), 3000);
        assert_eq!(ctrl.worst_loss_pct(), 0.0);
    }

    #[test]
    fn worst_loss_among_multiple_peers() {
        let mut ctrl = BitrateController::new(800, 3000);
        let t0 = Instant::now();
        ctrl.add_peer(1);
        ctrl.add_peer(2);
        ctrl.add_peer(3);

        ctrl.update_loss(1, 1.0, t0);
        ctrl.update_loss(2, 8.5, t0);
        ctrl.update_loss(3, 3.2, t0);

        // Pior perda é 8.5%
        assert_eq!(ctrl.worst_loss_pct(), 8.5);
        let health = ctrl.health();
        assert_eq!(health.target_kbps, 3000);
        assert_eq!(health.worst_loss_pct, 8.5);
    }
}
