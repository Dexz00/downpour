//! Nó DHT (BEP-5).
//!
//! Serve para duas coisas: descobrir peers de um info_hash sem depender de
//! tracker, e responder às consultas de outros nós — um DHT em que ninguém
//! responde não existe.

pub mod krpc;
pub mod routing;

use crate::id::Id20;
use anyhow::{Context, Result};
use krpc::{Message, NodeInfo, Query, Response};
use rand::RngExt;
use routing::{K, RoutingTable};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

/// Roteadores públicos usados para entrar na rede.
pub const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];

/// Consultas simultâneas durante uma busca (o "α" do Kademlia).
const ALPHA: usize = 8;
/// Quanto esperamos por uma resposta.
const QUERY_TIMEOUT: Duration = Duration::from_secs(4);
/// Rodadas máximas de uma busca iterativa, para ela sempre terminar.
const MAX_RODADAS: usize = 12;
/// Validade de um peer anunciado a nós.
const PEER_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Default)]
pub struct DhtStats {
    pub nodes: usize,
    pub good_nodes: usize,
    pub queries_sent: u64,
}

pub struct Dht {
    socket: UdpSocket,
    node_id: Id20,
    routing: Mutex<RoutingTable>,
    /// Consultas nossas ainda sem resposta, por transaction id.
    pending: Mutex<HashMap<u16, oneshot::Sender<Result<Response, String>>>>,
    next_tid: Mutex<u16>,
    /// Peers que outros nós anunciaram a nós.
    peer_store: Mutex<HashMap<Id20, Vec<(SocketAddr, Instant)>>>,
    /// Segredos usados para assinar tokens (o atual e o anterior).
    secrets: Mutex<[[u8; 16]; 2]>,
    stats: Estatisticas,
}

#[derive(Default)]
struct Estatisticas {
    queries_sent: AtomicU64,
    queries_received: AtomicU64,
    peers_found: AtomicU64,
}

impl Dht {
    /// Abre o socket UDP e sobe as tasks de recepção e manutenção.
    pub async fn bind(port: u16) -> Result<Arc<Dht>> {
        let socket = UdpSocket::bind(("0.0.0.0", port))
            .await
            .with_context(|| format!("não consegui abrir a porta UDP {port} para o DHT"))?;
        let node_id = Id20::random();

        let dht = Arc::new(Dht {
            socket,
            node_id,
            routing: Mutex::new(RoutingTable::new(node_id)),
            pending: Mutex::new(HashMap::new()),
            next_tid: Mutex::new(0),
            peer_store: Mutex::new(HashMap::new()),
            secrets: Mutex::new([rand::rng().random(), rand::rng().random()]),
            stats: Estatisticas::default(),
        });

        tokio::spawn(recv_loop(Arc::clone(&dht)));
        tokio::spawn(maintenance_loop(Arc::clone(&dht)));
        Ok(dht)
    }

    #[cfg(test)]
    pub fn node_id(&self) -> Id20 {
        self.node_id
    }

    #[cfg(test)]
    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn stats(&self) -> DhtStats {
        let routing = self.routing.lock().unwrap();
        DhtStats {
            nodes: routing.len(),
            good_nodes: routing.count_good(),
            queries_sent: self.stats.queries_sent.load(Ordering::Relaxed),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.routing.lock().unwrap().count_good() > 0
    }

    /// Nós conhecidos, para salvar entre execuções.
    pub fn snapshot(&self) -> Vec<(Id20, SocketAddr)> {
        self.routing.lock().unwrap().snapshot()
    }

    pub fn add_node(&self, info: NodeInfo) {
        self.routing.lock().unwrap().add(info);
    }

    /// Entra na rede perguntando por nós mesmos aos roteadores conhecidos.
    pub async fn bootstrap(self: &Arc<Self>, hosts: &[String]) -> usize {
        let mut tarefas = JoinSet::new();
        for host in hosts {
            let host = host.clone();
            let this = Arc::clone(self);
            tarefas.spawn(async move {
                let Ok(addrs) = lookup_host(&host).await else {
                    return 0;
                };
                let mut achados = 0;
                for addr in addrs.take(2) {
                    if let Ok(resp) = this
                        .query(
                            addr,
                            Query::FindNode {
                                id: this.node_id,
                                target: this.node_id,
                            },
                        )
                        .await
                    {
                        let mut routing = this.routing.lock().unwrap();
                        // O roteador em si não entra na tabela (ele não guarda
                        // dados); só os nós que ele indicou.
                        for no in &resp.nodes {
                            if routing.add(*no) {
                                achados += 1;
                            }
                        }
                    }
                }
                achados
            });
        }
        let mut total = 0;
        while let Some(r) = tarefas.join_next().await {
            total += r.unwrap_or(0);
        }
        crate::log::log!("dht", "bootstrap trouxe {total} nós");
        // Uma busca por um alvo aleatório espalha a tabela para vários baldes.
        if total > 0 {
            self.find_node(Id20::random()).await;
        }
        total
    }

    /// Busca iterativa por peers de um info_hash.
    ///
    /// Cada peer encontrado é despachado imediatamente pelo canal — o download
    /// pode começar enquanto a busca ainda caminha pela rede.
    pub async fn get_peers(
        self: &Arc<Self>,
        info_hash: Id20,
        peers_tx: Option<mpsc::Sender<Vec<SocketAddr>>>,
    ) -> Vec<(NodeInfo, Vec<u8>)> {
        let mut candidatos = Candidatos::new(info_hash);
        candidatos.oferecer(self.routing.lock().unwrap().closest(&info_hash, K * 2));
        let mut com_token: Vec<(NodeInfo, Vec<u8>)> = Vec::new();

        for _ in 0..MAX_RODADAS {
            let lote = candidatos.proximos(ALPHA);
            if lote.is_empty() {
                break;
            }

            let mut tarefas = JoinSet::new();
            for no in lote {
                let this = Arc::clone(self);
                tarefas.spawn(async move {
                    let r = this
                        .query(no.addr, Query::GetPeers { id: this.node_id, info_hash })
                        .await;
                    (no, r)
                });
            }

            let mut achou_algo = false;
            while let Some(Ok((no, resultado))) = tarefas.join_next().await {
                match resultado {
                    Ok(resp) => {
                        candidatos.marcar_ok(no.id);
                        achou_algo |= candidatos.oferecer(resp.nodes);
                        if let Some(token) = resp.token {
                            com_token.push((no, token));
                        }
                        if !resp.values.is_empty() {
                            self.stats
                                .peers_found
                                .fetch_add(resp.values.len() as u64, Ordering::Relaxed);
                            if let Some(tx) = &peers_tx
                                && tx.send(resp.values).await.is_err()
                            {
                                return com_token;
                            }
                        }
                    }
                    Err(_) => candidatos.marcar_falha(no.id),
                }
            }
            if !achou_algo && candidatos.esgotado() {
                break;
            }
        }

        crate::log::log!(
            "dht",
            "busca por {} terminou: {} nós com token, tabela com {} nós",
            &info_hash.to_hex()[..8],
            com_token.len(),
            self.routing.lock().unwrap().len()
        );
        // Só os mais próximos importam para anunciar depois.
        com_token.sort_unstable_by_key(|(no, _)| no.id.distance(&info_hash));
        com_token.truncate(K);
        com_token
    }

    /// Anuncia que temos este torrent, para outros nos encontrarem.
    pub async fn announce_peer(self: &Arc<Self>, info_hash: Id20, port: u16) -> usize {
        let alvos = self.get_peers(info_hash, None).await;
        let mut tarefas = JoinSet::new();
        for (no, token) in alvos {
            let this = Arc::clone(self);
            tarefas.spawn(async move {
                this.query(
                    no.addr,
                    Query::AnnouncePeer {
                        id: this.node_id,
                        info_hash,
                        port,
                        token,
                        implied_port: false,
                    },
                )
                .await
                .is_ok()
            });
        }
        let mut aceitos = 0;
        while let Some(r) = tarefas.join_next().await {
            aceitos += usize::from(r.unwrap_or(false));
        }
        aceitos
    }

    /// Busca por nós próximos de um alvo — usada no bootstrap e no refresh de
    /// baldes; o resultado alimenta a tabela de roteamento.
    async fn find_node(self: &Arc<Self>, target: Id20) {
        let mut candidatos = Candidatos::new(target);
        candidatos.oferecer(self.routing.lock().unwrap().closest(&target, K * 2));

        for _ in 0..MAX_RODADAS / 2 {
            let lote = candidatos.proximos(ALPHA);
            if lote.is_empty() {
                break;
            }
            let mut tarefas = JoinSet::new();
            for no in lote {
                let this = Arc::clone(self);
                tarefas.spawn(async move {
                    let r = this
                        .query(no.addr, Query::FindNode { id: this.node_id, target })
                        .await;
                    (no, r)
                });
            }
            while let Some(Ok((no, resultado))) = tarefas.join_next().await {
                match resultado {
                    Ok(resp) => {
                        candidatos.marcar_ok(no.id);
                        candidatos.oferecer(resp.nodes);
                    }
                    Err(_) => candidatos.marcar_falha(no.id),
                }
            }
        }
        for no in candidatos.vistos() {
            self.routing.lock().unwrap().add(no);
        }
    }

    /// Envia uma consulta e espera a resposta.
    async fn query(&self, addr: SocketAddr, query: Query) -> Result<Response, String> {
        let tid = {
            let mut n = self.next_tid.lock().unwrap();
            *n = n.wrapping_add(1);
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(tid, tx);

        let msg = Message::Query {
            transaction: tid.to_be_bytes().to_vec(),
            query,
        };
        let enviado = self.socket.send_to(&msg.encode(), addr).await;
        self.stats.queries_sent.fetch_add(1, Ordering::Relaxed);

        if let Err(e) = enviado {
            self.pending.lock().unwrap().remove(&tid);
            return Err(format!("envio falhou: {e}"));
        }

        let resultado = match tokio::time::timeout(QUERY_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("resposta descartada".to_string()),
            Err(_) => Err("sem resposta".to_string()),
        };
        // Em caso de timeout a entrada ainda está lá; sem isso o mapa cresceria
        // para sempre em uma rede com muitos nós mortos.
        self.pending.lock().unwrap().remove(&tid);
        resultado
    }

    /// Token que prova que o nó realmente está no IP que diz estar.
    fn make_token(&self, addr: &SocketAddr, indice: usize) -> Vec<u8> {
        let secret = self.secrets.lock().unwrap()[indice];
        let mut hasher = Sha1::new();
        hasher.update(secret);
        match addr.ip() {
            std::net::IpAddr::V4(ip) => hasher.update(ip.octets()),
            std::net::IpAddr::V6(ip) => hasher.update(ip.octets()),
        }
        hasher.finalize()[..8].to_vec()
    }

    /// Aceita o segredo atual e o anterior: um token emitido pouco antes da
    /// rotação continuaria válido para quem o recebeu.
    fn check_token(&self, addr: &SocketAddr, token: &[u8]) -> bool {
        (0..2).any(|i| self.make_token(addr, i) == token)
    }

    fn store_peer(&self, info_hash: Id20, addr: SocketAddr) {
        let mut store = self.peer_store.lock().unwrap();
        let lista = store.entry(info_hash).or_default();
        lista.retain(|(a, _)| *a != addr);
        lista.push((addr, Instant::now()));
        // Um info_hash popular não pode virar um vazamento de memória.
        if lista.len() > 128 {
            lista.remove(0);
        }
    }

    fn lookup_peers(&self, info_hash: Id20) -> Vec<SocketAddr> {
        let store = self.peer_store.lock().unwrap();
        store
            .get(&info_hash)
            .map(|lista| {
                lista
                    .iter()
                    .filter(|(_, t)| t.elapsed() < PEER_TTL)
                    .map(|(a, _)| *a)
                    .take(50)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Monta a resposta para uma consulta de outro nó.
    fn answer(&self, from: SocketAddr, query: &Query) -> Message {
        let responder = |response| Message::Response {
            transaction: Vec::new(),
            response,
        };
        match query {
            Query::Ping { .. } => responder(Response {
                id: self.node_id,
                ..Default::default()
            }),
            Query::FindNode { target, .. } => responder(Response {
                id: self.node_id,
                nodes: self.routing.lock().unwrap().closest(target, K),
                ..Default::default()
            }),
            Query::GetPeers { info_hash, .. } => {
                let peers = self.lookup_peers(*info_hash);
                responder(Response {
                    id: self.node_id,
                    // Quem tem peers manda peers; quem não tem manda o caminho.
                    nodes: if peers.is_empty() {
                        self.routing.lock().unwrap().closest(info_hash, K)
                    } else {
                        Vec::new()
                    },
                    values: peers,
                    token: Some(self.make_token(&from, 0)),
                })
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                token,
                implied_port,
                ..
            } => {
                if !self.check_token(&from, token) {
                    return Message::Error {
                        transaction: Vec::new(),
                        code: 203,
                        message: "token inválido".to_string(),
                    };
                }
                let porta = if *implied_port { from.port() } else { *port };
                if porta != 0 {
                    self.store_peer(*info_hash, SocketAddr::new(from.ip(), porta));
                }
                responder(Response {
                    id: self.node_id,
                    ..Default::default()
                })
            }
        }
    }
}

/// Conjunto de nós de uma busca, ordenado por distância ao alvo.
struct Candidatos {
    alvo: Id20,
    fila: Vec<NodeInfo>,
    consultados: HashSet<Id20>,
    vivos: HashSet<Id20>,
    vistos: HashSet<Id20>,
}

impl Candidatos {
    fn new(alvo: Id20) -> Candidatos {
        Candidatos {
            alvo,
            fila: Vec::new(),
            consultados: HashSet::new(),
            vivos: HashSet::new(),
            vistos: HashSet::new(),
        }
    }

    /// Adiciona nós ainda desconhecidos; devolve `true` se algum era novo.
    fn oferecer(&mut self, nodes: impl IntoIterator<Item = NodeInfo>) -> bool {
        let mut novidade = false;
        for no in nodes {
            if self.vistos.insert(no.id) {
                self.fila.push(no);
                novidade = true;
            }
        }
        if novidade {
            let alvo = self.alvo;
            self.fila.sort_unstable_by_key(|n| n.id.distance(&alvo));
            // A busca só precisa manter o horizonte próximo do alvo.
            self.fila.truncate(64);
        }
        novidade
    }

    fn proximos(&mut self, n: usize) -> Vec<NodeInfo> {
        let escolhidos: Vec<NodeInfo> = self
            .fila
            .iter()
            .filter(|no| !self.consultados.contains(&no.id))
            .take(n)
            .copied()
            .collect();
        for no in &escolhidos {
            self.consultados.insert(no.id);
        }
        escolhidos
    }

    fn marcar_ok(&mut self, id: Id20) {
        self.vivos.insert(id);
    }

    fn marcar_falha(&mut self, id: Id20) {
        self.fila.retain(|n| n.id != id);
    }

    fn esgotado(&self) -> bool {
        self.fila.iter().all(|n| self.consultados.contains(&n.id))
    }

    /// Nós que responderam — os únicos que merecem entrar na tabela.
    fn vistos(&self) -> Vec<NodeInfo> {
        self.fila
            .iter()
            .filter(|n| self.vivos.contains(&n.id))
            .copied()
            .collect()
    }
}

async fn recv_loop(dht: Arc<Dht>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, from) = match dht.socket.recv_from(&mut buf).await {
            Ok(r) => r,
            // ICMP port unreachable vira erro de recv no Windows; ignorar e
            // seguir, senão o DHT inteiro morre por causa de um nó offline.
            Err(_) => continue,
        };

        let Ok(msg) = Message::decode(&buf[..n]) else {
            continue;
        };
        match msg {
            Message::Query { transaction, query } => {
                dht.stats.queries_received.fetch_add(1, Ordering::Relaxed);
                // Quem fala conosco entra na tabela: é um nó comprovadamente vivo.
                if let Some(id) = query_id(&query) {
                    dht.routing.lock().unwrap().add(NodeInfo { id, addr: from });
                }
                let mut resposta = dht.answer(from, &query);
                match &mut resposta {
                    Message::Response { transaction: t, .. }
                    | Message::Error { transaction: t, .. } => *t = transaction,
                    Message::Query { .. } => unreachable!(),
                }
                let _ = dht.socket.send_to(&resposta.encode(), from).await;
            }
            Message::Response {
                transaction,
                response,
            } => {
                {
                    let mut routing = dht.routing.lock().unwrap();
                    routing.add(NodeInfo {
                        id: response.id,
                        addr: from,
                    });
                    routing.touch(&response.id);
                }
                if let Some(tx) = take_pending(&dht, &transaction) {
                    let _ = tx.send(Ok(response));
                }
            }
            Message::Error {
                transaction,
                code,
                message,
            } => {
                if let Some(tx) = take_pending(&dht, &transaction) {
                    let _ = tx.send(Err(format!("erro {code}: {message}")));
                }
            }
        }
    }
}

fn take_pending(
    dht: &Dht,
    transaction: &[u8],
) -> Option<oneshot::Sender<Result<Response, String>>> {
    let tid = u16::from_be_bytes(transaction.get(..2)?.try_into().ok()?);
    dht.pending.lock().unwrap().remove(&tid)
}

fn query_id(query: &Query) -> Option<Id20> {
    Some(match query {
        Query::Ping { id }
        | Query::FindNode { id, .. }
        | Query::GetPeers { id, .. }
        | Query::AnnouncePeer { id, .. } => *id,
    })
}

async fn maintenance_loop(dht: Arc<Dht>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut ciclos = 0u32;
    loop {
        tick.tick().await;
        ciclos += 1;

        // Rotaciona o segredo dos tokens a cada 5 minutos.
        if ciclos.is_multiple_of(5) {
            let mut secrets = dht.secrets.lock().unwrap();
            secrets[1] = secrets[0];
            secrets[0] = rand::rng().random();
        }

        // Descarta peers vencidos.
        {
            let mut store = dht.peer_store.lock().unwrap();
            for lista in store.values_mut() {
                lista.retain(|(_, t)| t.elapsed() < PEER_TTL);
            }
            store.retain(|_, lista| !lista.is_empty());
        }

        // Confere os nós duvidosos antes de perdê-los.
        let duvidosos = dht.routing.lock().unwrap().questionable();
        for no in duvidosos.into_iter().take(ALPHA) {
            let this = Arc::clone(&dht);
            tokio::spawn(async move {
                if this.query(no.addr, Query::Ping { id: this.node_id }).await.is_err() {
                    this.routing.lock().unwrap().mark_failed(&no.id);
                }
            });
        }

        // Reanima baldes parados.
        let alvos = dht.routing.lock().unwrap().refresh_targets();
        for alvo in alvos.into_iter().take(2) {
            let this = Arc::clone(&dht);
            tokio::spawn(async move {
                this.find_node(alvo).await;
                this.routing.lock().unwrap().mark_bucket_refreshed(&alvo);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn dht_local() -> Arc<Dht> {
        Dht::bind(0).await.expect("bind em porta efêmera")
    }

    #[tokio::test]
    async fn responde_ping_e_find_node() {
        let a = dht_local().await;
        let b = dht_local().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));

        let resp = a
            .query(addr_b, Query::Ping { id: a.node_id() })
            .await
            .expect("ping deve responder");
        assert_eq!(resp.id, b.node_id());

        // O ping fez `b` conhecer `a`.
        assert!(b.stats().nodes >= 1, "quem fala conosco entra na tabela");

        let resp = a
            .query(
                addr_b,
                Query::FindNode {
                    id: a.node_id(),
                    target: Id20::random(),
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.id, b.node_id());
    }

    #[tokio::test]
    async fn get_peers_e_announce_com_token() {
        let a = dht_local().await;
        let b = dht_local().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));
        let info_hash = Id20::random();

        // Sem ninguém anunciado, vem token e nós (nenhum, aqui) mas sem peers.
        let resp = a
            .query(
                addr_b,
                Query::GetPeers {
                    id: a.node_id(),
                    info_hash,
                },
            )
            .await
            .unwrap();
        let token = resp.token.clone().expect("get_peers devolve token");
        assert!(resp.values.is_empty());

        let ok = a
            .query(
                addr_b,
                Query::AnnouncePeer {
                    id: a.node_id(),
                    info_hash,
                    port: 51413,
                    token,
                    implied_port: false,
                },
            )
            .await;
        assert!(ok.is_ok(), "announce com token válido é aceito");

        // Agora `b` conhece um peer para esse info_hash.
        let resp = a
            .query(
                addr_b,
                Query::GetPeers {
                    id: a.node_id(),
                    info_hash,
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.values.len(), 1);
        assert_eq!(resp.values[0].port(), 51413);
    }

    #[tokio::test]
    async fn recusa_announce_com_token_falso() {
        let a = dht_local().await;
        let b = dht_local().await;
        let addr_b = SocketAddr::from(([127, 0, 0, 1], b.local_port()));

        let erro = a
            .query(
                addr_b,
                Query::AnnouncePeer {
                    id: a.node_id(),
                    info_hash: Id20::random(),
                    port: 6881,
                    token: b"tokenfalso".to_vec(),
                    implied_port: false,
                },
            )
            .await
            .unwrap_err();
        assert!(erro.contains("203"), "erro veio: {erro}");
    }

    #[tokio::test]
    async fn consulta_sem_resposta_expira_e_limpa() {
        let a = dht_local().await;
        // Porta onde ninguém escuta.
        let morto = SocketAddr::from(([127, 0, 0, 1], 1));
        let r = tokio::time::timeout(
            QUERY_TIMEOUT + Duration::from_secs(2),
            a.query(morto, Query::Ping { id: a.node_id() }),
        )
        .await
        .expect("a consulta precisa desistir sozinha");
        assert!(r.is_err());
        assert!(a.pending.lock().unwrap().is_empty(), "sem vazamento");
    }

    #[test]
    fn candidatos_ordenam_e_nao_repetem() {
        let alvo = Id20([0xff; 20]);
        let mut c = Candidatos::new(alvo);
        let perto = NodeInfo {
            id: Id20([0xfe; 20]),
            addr: "1.1.1.1:1".parse().unwrap(),
        };
        let longe = NodeInfo {
            id: Id20([0x00; 20]),
            addr: "2.2.2.2:2".parse().unwrap(),
        };

        assert!(c.oferecer([longe, perto]));
        assert!(!c.oferecer([perto]), "nó repetido não conta como novidade");

        let lote = c.proximos(1);
        assert_eq!(lote[0].id, perto.id, "o mais próximo do alvo vai primeiro");
        assert_eq!(c.proximos(1)[0].id, longe.id, "não repete consulta");
        assert!(c.esgotado());

        c.marcar_ok(perto.id);
        assert_eq!(c.vistos().len(), 1, "só quem respondeu conta");
    }
}
