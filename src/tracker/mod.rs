//! Coordenação dos trackers.
//!
//! Segue a BEP-12: os trackers vêm em *tiers*, e dentro de cada tier só o
//! primeiro que responder é usado — o que funcionou sobe para o topo. Cada tier
//! roda em sua própria task e despeja peers no engine por um canal.

pub mod http;
pub mod udp;

use crate::id::Id20;
use anyhow::{Result, bail};
use rand::RngExt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Started,
    Completed,
    Stopped,
    /// Announce de rotina, sem evento associado.
    Periodic,
}

#[derive(Debug, Clone)]
pub struct AnnounceRequest {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
    pub num_want: i32,
    /// Identifica o cliente entre trocas de IP (BEP-3).
    pub key: u32,
    pub tracker_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnnounceResponse {
    pub interval: Duration,
    pub min_interval: Option<Duration>,
    pub peers: Vec<SocketAddr>,
    pub seeders: u32,
    pub leechers: u32,
    pub tracker_id: Option<String>,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerState {
    Aguardando,
    Consultando,
    Ok,
    Falhou,
    Desligado,
}

#[derive(Debug, Clone)]
pub struct TrackerStatus {
    pub url: String,
    pub tier: usize,
    pub state: TrackerState,
    pub message: Option<String>,
    pub seeders: u32,
    pub leechers: u32,
    pub peers: usize,
    /// Segundos até o próximo announce.
    pub next_in: u64,
}

/// Contadores que o engine mantém atualizados e os trackers leem na hora do
/// announce.
#[derive(Default)]
pub struct Counters {
    pub uploaded: AtomicU64,
    pub downloaded: AtomicU64,
    pub left: AtomicU64,
}

impl Counters {
    pub fn set(&self, uploaded: u64, downloaded: u64, left: u64) {
        self.uploaded.store(uploaded, Ordering::Relaxed);
        self.downloaded.store(downloaded, Ordering::Relaxed);
        self.left.store(left, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct TrackerConfig {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub port: u16,
    pub num_want: i32,
    pub client: reqwest::Client,
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Announce(Event),
    Shutdown,
}

/// Alça do engine para o conjunto de trackers.
pub struct Trackers {
    cfg: TrackerConfig,
    counters: Arc<Counters>,
    status: Arc<Mutex<Vec<TrackerStatus>>>,
    cmds: broadcast::Sender<Command>,
    peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    tiers: Mutex<usize>,
}

impl Trackers {
    pub fn new(
        cfg: TrackerConfig,
        tiers: Vec<Vec<String>>,
        peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    ) -> Arc<Trackers> {
        let (cmds, _) = broadcast::channel(16);
        let trackers = Arc::new(Trackers {
            cfg,
            counters: Arc::new(Counters::default()),
            status: Arc::new(Mutex::new(Vec::new())),
            cmds,
            peers_tx,
            tiers: Mutex::new(0),
        });
        for tier in tiers {
            trackers.add_tier(tier);
        }
        trackers
    }

    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    /// Sobe uma nova task de tier. Usado também para trackers que aparecem
    /// depois, vindos de um magnet.
    pub fn add_tier(self: &Arc<Self>, urls: Vec<String>) {
        let urls: Vec<String> = urls
            .into_iter()
            .filter(|u| u.starts_with("http://") || u.starts_with("https://") || u.starts_with("udp://"))
            .collect();
        if urls.is_empty() {
            return;
        }
        // Um tracker já conhecido não ganha uma segunda task.
        {
            let atual = self.status.lock().unwrap();
            if urls.iter().all(|u| atual.iter().any(|s| s.url == *u)) {
                return;
            }
        }

        let tier = {
            let mut n = self.tiers.lock().unwrap();
            *n += 1;
            *n - 1
        };
        {
            let mut status = self.status.lock().unwrap();
            for url in &urls {
                status.push(TrackerStatus {
                    url: url.clone(),
                    tier,
                    state: TrackerState::Aguardando,
                    message: None,
                    seeders: 0,
                    leechers: 0,
                    peers: 0,
                    next_in: 0,
                });
            }
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            tier_loop(this, tier, urls).await;
        });
    }

    pub fn announce(&self, event: Event) {
        let _ = self.cmds.send(Command::Announce(event));
    }

    pub fn shutdown(&self) {
        let _ = self.cmds.send(Command::Shutdown);
    }

    pub fn status(&self) -> Vec<TrackerStatus> {
        self.status.lock().unwrap().clone()
    }

    /// Avisa os trackers que estamos saindo, com prazo curto — ninguém espera
    /// meio minuto para fechar o programa.
    pub async fn announce_stopped(&self, prazo: Duration) {
        let urls: Vec<String> = {
            let status = self.status.lock().unwrap();
            status
                .iter()
                .filter(|s| s.state == TrackerState::Ok)
                .map(|s| s.url.clone())
                .collect()
        };
        let req = self.build_request(Event::Stopped);
        let tarefas: Vec<_> = urls
            .into_iter()
            .map(|url| {
                let cfg = self.cfg.clone();
                let req = req.clone();
                tokio::spawn(async move { announce_one(&cfg, &url, &req).await })
            })
            .collect();
        let _ = tokio::time::timeout(prazo, async {
            for t in tarefas {
                let _ = t.await;
            }
        })
        .await;
    }

    fn build_request(&self, event: Event) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: self.cfg.info_hash,
            peer_id: self.cfg.peer_id,
            port: self.cfg.port,
            uploaded: self.counters.uploaded.load(Ordering::Relaxed),
            downloaded: self.counters.downloaded.load(Ordering::Relaxed),
            left: self.counters.left.load(Ordering::Relaxed),
            event,
            num_want: self.cfg.num_want,
            key: 0,
            tracker_id: None,
        }
    }

    fn update<F: FnOnce(&mut TrackerStatus)>(&self, url: &str, f: F) {
        let mut status = self.status.lock().unwrap();
        if let Some(s) = status.iter_mut().find(|s| s.url == url) {
            f(s);
        }
    }
}

/// Espera aplicada depois de `n` falhas seguidas, com um pouco de aleatório
/// para os trackers não voltarem todos no mesmo segundo.
fn backoff(falhas: u32) -> Duration {
    let base = 30u64.saturating_mul(1 << falhas.min(6));
    let jitter = rand::rng().random_range(0..=base / 4);
    Duration::from_secs((base + jitter).min(30 * 60))
}

async fn tier_loop(trackers: Arc<Trackers>, tier: usize, mut urls: Vec<String>) {
    let mut cmds = trackers.cmds.subscribe();
    let mut evento = Event::Started;
    let mut key = rand::rng().random::<u32>();
    let mut proximo = Instant::now();
    let mut falhas = 0u32;
    // O tracker pode nos dar um identificador de sessão para reapresentarmos
    // nos próximos announces, e um piso de intervalo que vale até para os
    // announces disparados por evento.
    let mut tracker_id: Option<String> = None;
    let mut minimo: Option<Duration> = None;
    let mut ultimo: Option<Instant> = None;

    loop {
        let espera = proximo.saturating_duration_since(Instant::now());
        {
            // Deixa a UI mostrar a contagem regressiva.
            let mut status = trackers.status.lock().unwrap();
            for s in status.iter_mut().filter(|s| s.tier == tier) {
                s.next_in = espera.as_secs();
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(espera) => {}
            cmd = cmds.recv() => match cmd {
                Ok(Command::Announce(e)) => {
                    evento = e;
                    // Respeitar o piso do tracker evita levar um "too many
                    // requests" logo depois de trocar de estado.
                    proximo = match (ultimo, minimo) {
                        (Some(t), Some(m)) => Instant::now().max(t + m),
                        _ => Instant::now(),
                    };
                    continue;
                }
                Ok(Command::Shutdown) | Err(broadcast::error::RecvError::Closed) => {
                    let mut status = trackers.status.lock().unwrap();
                    for s in status.iter_mut().filter(|s| s.tier == tier) {
                        s.state = TrackerState::Desligado;
                    }
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }

        let mut req = trackers.build_request(evento);
        req.key = key;
        req.tracker_id = tracker_id.clone();
        ultimo = Some(Instant::now());

        let mut deu_certo = false;
        for i in 0..urls.len() {
            let url = urls[i].clone();
            trackers.update(&url, |s| {
                s.state = TrackerState::Consultando;
                s.message = None;
            });

            match announce_one(&trackers.cfg, &url, &req).await {
                Ok(resposta) => {
                    trackers.update(&url, |s| {
                        s.state = TrackerState::Ok;
                        s.seeders = resposta.seeders;
                        s.leechers = resposta.leechers;
                        s.peers = resposta.peers.len();
                        s.message = resposta.warning.clone();
                    });
                    if !resposta.peers.is_empty()
                        && trackers.peers_tx.send(resposta.peers).await.is_err()
                    {
                        return; // engine encerrou
                    }
                    // BEP-12: o tracker que respondeu vira o primeiro do tier.
                    urls.swap(0, i);
                    if resposta.tracker_id.is_some() {
                        tracker_id = resposta.tracker_id.clone();
                    }
                    minimo = resposta.min_interval;
                    proximo = Instant::now() + resposta.interval;
                    falhas = 0;
                    deu_certo = true;
                    break;
                }
                Err(e) => {
                    trackers.update(&url, |s| {
                        s.state = TrackerState::Falhou;
                        s.message = Some(format!("{e:#}"));
                    });
                }
            }
        }

        if !deu_certo {
            falhas += 1;
            proximo = Instant::now() + backoff(falhas);
            // Uma nova identidade evita ficar preso a um ban por `key`.
            if falhas.is_multiple_of(4) {
                key = rand::rng().random::<u32>();
            }
        }
        if evento == Event::Stopped {
            // Já avisamos que saímos; ficar reanunciando "stopped" de meia em
            // meia hora não faz sentido. O laço volta a agir quando o engine
            // mandar um novo announce.
            proximo = Instant::now() + Duration::from_secs(24 * 3600);
        } else {
            // Depois do primeiro announce, os seguintes são de rotina.
            evento = Event::Periodic;
        }
    }
}

async fn announce_one(
    cfg: &TrackerConfig,
    url: &str,
    req: &AnnounceRequest,
) -> Result<AnnounceResponse> {
    if url.starts_with("udp://") {
        udp::announce(url, req).await
    } else if url.starts_with("http://") || url.starts_with("https://") {
        http::announce(&cfg.client, url, req).await
    } else {
        bail!("esquema de tracker não suportado: {url}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_cresce_e_tem_teto() {
        let primeiro = backoff(1).as_secs();
        assert!((60..=75).contains(&primeiro), "60s + jitter, veio {primeiro}");
        assert!(backoff(3).as_secs() > backoff(1).as_secs());
        assert_eq!(backoff(30).as_secs(), 30 * 60, "não passa de 30 minutos");
    }

    fn trackers_de_teste(tiers: Vec<Vec<String>>) -> (Arc<Trackers>, mpsc::Receiver<Vec<SocketAddr>>) {
        let (tx, rx) = mpsc::channel(8);
        let cfg = TrackerConfig {
            info_hash: Id20::random(),
            peer_id: Id20::random(),
            port: 6881,
            num_want: 200,
            client: reqwest::Client::new(),
        };
        (Trackers::new(cfg, tiers, tx), rx)
    }

    #[tokio::test]
    async fn registra_tiers_e_ignora_esquemas_estranhos() {
        let (trackers, _rx) = trackers_de_teste(vec![
            vec!["http://a/ann".into(), "udp://b:6969".into()],
            vec!["wss://c/ann".into()],
        ]);
        let status = trackers.status();
        assert_eq!(status.len(), 2, "o tier só de websocket é descartado");
        assert_eq!(status[0].tier, 0);
        assert_eq!(status[1].tier, 0);
        assert!(status.iter().all(|s| s.state != TrackerState::Desligado));

        // Um tracker repetido não vira uma segunda task.
        trackers.add_tier(vec!["http://a/ann".into()]);
        assert_eq!(trackers.status().len(), 2);

        trackers.add_tier(vec!["http://novo/ann".into()]);
        let status = trackers.status();
        assert_eq!(status.len(), 3);
        assert_eq!(status[2].tier, 1, "tracker novo entra em um tier próprio");
    }

    #[tokio::test]
    async fn contadores_entram_no_pedido() {
        let (trackers, _rx) = trackers_de_teste(vec![]);
        trackers.counters().set(111, 222, 333);
        let req = trackers.build_request(Event::Completed);
        assert_eq!((req.uploaded, req.downloaded, req.left), (111, 222, 333));
        assert_eq!(req.event, Event::Completed);
    }
}

#[cfg(test)]
mod testes_rede {
    use super::*;
    use crate::metainfo::Metainfo;

    /// Diagnóstico manual: `cargo test announce_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn announce_real() {
        let caminho = std::env::var("TORRENT").expect("defina TORRENT=caminho.torrent");
        let meta = Metainfo::from_file(&caminho).unwrap();
        let cfg = TrackerConfig {
            info_hash: meta.info.info_hash,
            peer_id: crate::id::generate_peer_id(),
            port: 6881,
            num_want: 200,
            client: reqwest::Client::builder()
                .user_agent("torrra/0.1.0")
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        };
        let req = AnnounceRequest {
            info_hash: cfg.info_hash,
            peer_id: cfg.peer_id,
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: meta.info.total_length,
            event: Event::Started,
            num_want: 200,
            key: 12345,
            tracker_id: None,
        };
        for tier in &meta.tiers {
            for url in tier {
                println!("--> {url}");
                match announce_one(&cfg, url, &req).await {
                    Ok(r) => println!("    OK: {} peers, {} seeds", r.peers.len(), r.seeders),
                    Err(e) => println!("    ERRO: {e:#}"),
                }
            }
        }
    }
}
