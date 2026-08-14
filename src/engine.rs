//! O engine de um torrent: junta trackers, DHT, peers, picker e disco.
//!
//! Tudo que decide política mora aqui e roda em uma única task. Os peers são
//! tasks separadas que só falam protocolo; o disco roda em `spawn_blocking`.
//! Assim o laço de decisão nunca bloqueia, e não existe estado compartilhado
//! entre peers para dar errado.

use crate::bitfield::Bitfield;
use crate::compact;
use crate::dht::Dht;
use crate::id::{Id20, generate_peer_id};
use crate::magnet::Magnet;
use crate::metainfo::{Info, Metainfo};
use crate::peer::extension::{
    ExtensionHandshake, METADATA_PIECE, MetadataBuffer, MetadataMessage, PexMessage,
};
use crate::peer::message::Handshake;
use crate::peer::{self, PeerCommand, PeerEvent, PeerHandle, Slot};
use crate::picker::{BLOCK_SIZE, BlockRequest, PickMode, Picker, Priority};
use crate::stats::{History, RateMeter};
use crate::storage::Storage;
use crate::tracker::{Event, TrackerConfig, TrackerStatus, Trackers};
use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Ritmo do laço de decisão.
const TICK: Duration = Duration::from_millis(250);
/// Frequência do algoritmo de choking (BEP-3 recomenda 10 s).
const CHOKE_INTERVALO: Duration = Duration::from_secs(10);
/// A cada quantos ciclos de choke trocamos o unchoke otimista.
const OPTIMISTIC_CICLOS: u32 = 3;
/// Um peer que aceitou pedidos e não entrega nada por esse tempo está "snubbed".
const SNUB_TIMEOUT: Duration = Duration::from_secs(30);
/// Prazo para um bloco pedido voltar ao pool.
const BLOCO_TIMEOUT: Duration = Duration::from_secs(45);
/// Intervalo entre buscas no DHT quando o enxame já está saudável.
const DHT_INTERVALO: Duration = Duration::from_secs(15 * 60);
/// Intervalo enquanto ainda faltam peers ou o DHT está se formando.
const DHT_RETENTATIVA: Duration = Duration::from_secs(60);
/// Pedidos em voo por peer, no mínimo e no máximo.
const QUEUE_MIN: usize = 4;
const QUEUE_MAX: usize = 500;
/// Conexões simultâneas em aberto (ainda sem handshake).
///
/// A maior parte dos endereços que o DHT devolve está morta ou atrás de NAT,
/// então o custo de discar é tempo de espera, não banda. Discar em largura com
/// prazo curto encontra os peers vivos muito mais rápido do que uma fila
/// estreita com prazo generoso.
const MAX_CONECTANDO: usize = 60;
/// Prazo para o TCP abrir antes de desistir do candidato.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// Intervalo entre trocas de peers por PEX. A BEP-11 pede no mínimo 60 s.
const PEX_INTERVALO: Duration = Duration::from_secs(90);
/// Teto de endereços por mensagem PEX, para não estourar o pacote.
const PEX_MAX: usize = 50;
/// Tamanho máximo da fila de endereços a tentar.
const MAX_CANDIDATOS: usize = 2000;
/// Quanto um endereço fica fora da fila enquanto está nela ou conectado.
const RESERVA: Duration = Duration::from_secs(60 * 60);
/// Espera antes de tentar de novo um peer que chegou a conversar conosco.
const REENTRADA_BOA: Duration = Duration::from_secs(60);
/// Espera antes de tentar de novo um endereço que nem completou o handshake.
const REENTRADA_RUIM: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct Config {
    pub dest: PathBuf,
    pub max_peers: usize,
    pub upload_slots: usize,
    pub listen_port: u16,
    pub sequential: bool,
    /// Limites em bytes/s; `None` = sem limite.
    pub max_download: Option<u64>,
    pub max_upload: Option<u64>,
    pub use_dht: bool,
    /// Parar de enviar dados assim que o download terminar.
    pub seed_after_complete: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            dest: PathBuf::from("."),
            max_peers: 100,
            upload_slots: 6,
            listen_port: 6881,
            sequential: false,
            max_download: None,
            max_upload: None,
            use_dht: true,
            seed_after_complete: true,
        }
    }
}

/// De onde o torrent veio.
pub enum Source {
    Torrent(Box<Metainfo>),
    Magnet(Box<Magnet>),
}

impl Source {
    pub fn info_hash(&self) -> Id20 {
        match self {
            Source::Torrent(m) => m.info.info_hash,
            Source::Magnet(m) => m.info_hash,
        }
    }

    pub fn name(&self) -> String {
        match self {
            Source::Torrent(m) => m.info.name.clone(),
            Source::Magnet(m) => m
                .name
                .clone()
                .unwrap_or_else(|| format!("magnet:{}", &m.info_hash.to_hex()[..16])),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Magnet: baixando o dicionário `info` dos peers.
    BuscandoMetadata,
    /// Conferindo o que já existe no disco.
    Verificando,
    Baixando,
    Semeando,
    Pausado,
    Erro,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::BuscandoMetadata => "metadata",
            State::Verificando => "verificando",
            State::Baixando => "baixando",
            State::Semeando => "semeando",
            State::Pausado => "pausado",
            State::Erro => "erro",
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Pause,
    Resume,
    SetMode(PickMode),
    SetFilePriority(usize, Priority),
    /// Conexão recebida cujo handshake o listener já validou.
    Incoming(Box<(TcpStream, SocketAddr, Handshake)>),
    Recheck,
    Shutdown,
}

// -- snapshot para a interface --------------------------------------------

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub addr: SocketAddr,
    pub client: String,
    pub progress: f64,
    pub download_rate: f64,
    pub upload_rate: f64,
    pub choking_us: bool,
    pub interested_in_us: bool,
    pub we_choke: bool,
    pub in_flight: usize,
    pub snubbed: bool,
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub length: u64,
    pub downloaded: u64,
    pub priority: Priority,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub name: String,
    pub info_hash: String,
    pub state_label: String,
    pub error: Option<String>,
    pub have_pieces: usize,
    pub total_pieces: usize,
    /// Bitfield das peças que temos, para a barra de progresso detalhada.
    pub pieces: Vec<u8>,
    /// Peças abertas no momento, com fração concluída.
    pub partials: Vec<(u32, f32)>,
    pub downloaded: u64,
    pub uploaded: u64,
    pub wanted: u64,
    pub total_size: u64,
    pub download_rate: f64,
    pub upload_rate: f64,
    pub eta: Option<u64>,
    pub peers: Vec<PeerInfo>,
    pub connected: usize,
    pub candidates: usize,
    pub seeds_in_swarm: u32,
    pub peers_in_swarm: u32,
    pub files: Vec<FileInfo>,
    pub trackers: Vec<TrackerStatus>,
    pub dht_nodes: usize,
    /// Nós do DHT que responderam recentemente.
    pub dht_good: usize,
    pub dht_queries: u64,
    /// Porta em que estamos escutando de fato.
    pub listen_port: u16,
    pub down_history: Vec<f64>,
    pub up_history: Vec<f64>,
    pub sequential: bool,
    pub endgame: bool,
    /// Progresso da verificação inicial, quando aplicável.
    pub check_progress: f64,
}

pub struct TorrentHandle {
    pub info_hash: Id20,
    cmds: mpsc::Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl TorrentHandle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn try_send(&self, cmd: Command) {
        let _ = self.cmds.try_send(cmd);
    }
}

// -- estado interno --------------------------------------------------------

/// Fila de endereços a discar, com quarentena por tempo.
///
/// A quarentena substitui um simples "já vi este endereço": marcar endereços
/// como vistos para sempre parece evitar duplicatas, mas na prática queima o
/// enxame — todo peer que cai fica banido, e depois de pausar e retomar não
/// sobra ninguém para discar, porque o tracker devolve exatamente os mesmos
/// endereços de antes.
#[derive(Default)]
struct Candidatos {
    fila: VecDeque<SocketAddr>,
    /// Endereço -> instante a partir do qual ele pode voltar para a fila.
    quarentena: HashMap<SocketAddr, Instant>,
}

impl Candidatos {
    /// Enfileira o endereço se ele for discável e não estiver de quarentena.
    fn oferecer(&mut self, addr: SocketAddr) -> bool {
        if !compact::is_routable(&addr) {
            return false;
        }
        let agora = Instant::now();
        if self.quarentena.get(&addr).is_some_and(|t| *t > agora) {
            return false;
        }
        // Enquanto estiver na fila (ou conectado) o endereço fica reservado;
        // é isso que impede duplicatas.
        self.quarentena.insert(addr, agora + RESERVA);
        self.fila.push_back(addr);
        if self.fila.len() > MAX_CANDIDATOS
            && let Some(velho) = self.fila.pop_front()
        {
            self.quarentena.remove(&velho);
        }
        true
    }

    fn proximo(&mut self) -> Option<SocketAddr> {
        self.fila.pop_front()
    }

    /// O peer caiu: libera o endereço para uma nova tentativa mais tarde.
    fn devolver(&mut self, addr: SocketAddr, espera: Duration) {
        self.quarentena.insert(addr, Instant::now() + espera);
    }

    /// Libera tudo agora — usado ao pausar, para que retomar volte a discar
    /// imediatamente em vez de esperar o próximo announce.
    fn liberar_tudo(&mut self) {
        self.fila.clear();
        let agora = Instant::now();
        for quando in self.quarentena.values_mut() {
            *quando = agora;
        }
    }

    /// Recoloca na fila os endereços cuja quarentena já venceu.
    fn reenfileirar(&mut self) {
        let agora = Instant::now();
        let liberados: Vec<SocketAddr> = self
            .quarentena
            .iter()
            .filter(|(_, quando)| **quando <= agora)
            .map(|(addr, _)| *addr)
            .take(MAX_CANDIDATOS)
            .collect();
        for addr in liberados {
            self.oferecer(addr);
        }
    }

    fn len(&self) -> usize {
        self.fila.len()
    }
}

struct PeerState {
    handle: PeerHandle,
    addr: SocketAddr,
    client: String,
    bitfield: Bitfield,
    /// Peer que mandou `have all` antes de sabermos quantas peças existem.
    have_all: bool,
    /// Peças anunciadas antes de termos o metadata.
    pendentes: Vec<u32>,
    contabilizado: bool,
    /// O handshake terminou? Enquanto for `false`, este peer ocupa uma vaga
    /// em `conectando`.
    handshake_ok: bool,
    /// Fomos nós que discamos? Só esses endereços servem para o PEX: em uma
    /// conexão recebida, a porta de origem não é a porta em que o peer escuta.
    discamos: bool,
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    in_flight: Vec<BlockRequest>,
    queue_depth: usize,
    down: RateMeter,
    up: RateMeter,
    extensions: ExtensionHandshake,
    metadata_pedido: Option<Instant>,
    /// O peer anunciou suporte a DHT no handshake.
    fala_dht: bool,
    ultimo_bloco: Instant,
    conectado_em: Instant,
    snubbed: bool,
}

impl PeerState {
    fn progresso(&self) -> f64 {
        if self.have_all {
            return 1.0;
        }
        if self.bitfield.is_empty() {
            return 0.0;
        }
        self.bitfield.count() as f64 / self.bitfield.len() as f64
    }

    fn is_seed(&self) -> bool {
        self.have_all || (!self.bitfield.is_empty() && self.bitfield.is_complete())
    }
}

/// Estado que só existe depois que o `info` é conhecido.
struct Download {
    info: Arc<Info>,
    storage: Arc<Storage>,
    picker: Picker,
}

enum DiskEvent {
    Verificado {
        piece: u32,
        ok: bool,
    },
    ErroDeEscrita(String),
    VerificacaoPronta(Box<Bitfield>),
    BlocoLido {
        slot: Slot,
        request: BlockRequest,
        data: Vec<u8>,
    },
}

struct Engine {
    cfg: Config,
    info_hash: Id20,
    peer_id: Id20,
    name: String,
    state: State,
    erro: Option<String>,

    download: Option<Download>,
    metadata: Option<MetadataBuffer>,
    metadata_size: Option<usize>,
    private: bool,

    peers: HashMap<Slot, PeerState>,
    proximo_slot: Slot,
    candidatos: Candidatos,
    conectando: usize,

    trackers: Arc<Trackers>,
    dht: Option<Arc<Dht>>,
    proximo_dht: Instant,
    /// Evita disparar várias buscas DHT ao mesmo tempo.
    dht_ocupado: Arc<std::sync::atomic::AtomicBool>,
    proximo_pex: Instant,
    /// Endereços já anunciados por PEX, para mandar só a diferença.
    pex_enviados: HashSet<SocketAddr>,

    down: RateMeter,
    up: RateMeter,
    hist_down: History,
    hist_up: History,
    ultimo_hist: Instant,

    ciclo_choke: u32,
    proximo_choke: Instant,
    otimista: Option<Slot>,
    check_progress: Arc<AtomicUsize>,

    eventos_tx: mpsc::Sender<PeerEvent>,
    disco_tx: mpsc::Sender<DiskEvent>,
    /// Mesma ponta que os trackers usam; o DHT também despeja peers aqui.
    peers_tx: mpsc::Sender<Vec<SocketAddr>>,
    snapshot: Arc<Mutex<Snapshot>>,
}

/// Sobe o engine de um torrent e devolve a alça de controle.
pub fn spawn(source: Source, cfg: Config, dht: Option<Arc<Dht>>) -> TorrentHandle {
    let info_hash = source.info_hash();
    let (cmds_tx, cmds_rx) = mpsc::channel(64);
    let snapshot = Arc::new(Mutex::new(Snapshot {
        name: source.name(),
        info_hash: info_hash.to_hex(),
        state_label: "iniciando".to_string(),
        ..Default::default()
    }));

    let handle = TorrentHandle {
        info_hash,
        cmds: cmds_tx,
        snapshot: Arc::clone(&snapshot),
    };

    tokio::spawn(async move {
        if let Err(e) = run(source, cfg, dht, cmds_rx, Arc::clone(&snapshot)).await {
            let mut snap = snapshot.lock().unwrap();
            snap.state_label = State::Erro.label().to_string();
            snap.error = Some(format!("{e:#}"));
        }
    });
    handle
}

async fn run(
    source: Source,
    cfg: Config,
    dht: Option<Arc<Dht>>,
    mut cmds: mpsc::Receiver<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
) -> Result<()> {
    let info_hash = source.info_hash();
    let peer_id = generate_peer_id();
    let (eventos_tx, mut eventos_rx) = mpsc::channel(4096);
    let (disco_tx, mut disco_rx) = mpsc::channel(256);
    let (peers_tx, mut peers_rx) = mpsc::channel(64);

    let (tiers, metainfo, magnet_peers, private, nos_dht) = match source {
        Source::Torrent(m) => {
            let privado = m.info.private;
            (
                m.tiers.clone(),
                Some(m.info.clone()),
                Vec::new(),
                privado,
                m.dht_nodes.clone(),
            )
        }
        Source::Magnet(m) => (
            m.trackers.iter().map(|t| vec![t.clone()]).collect(),
            None,
            m.peers.clone(),
            false,
            Vec::new(),
        ),
    };

    // Torrents podem trazer nós DHT de partida (BEP-5); aproveitá-los encurta
    // o bootstrap, principalmente em enxames pequenos.
    if let Some(dht) = dht.as_ref().filter(|_| !private && !nos_dht.is_empty()) {
        let dht = Arc::clone(dht);
        tokio::spawn(async move {
            for host in nos_dht {
                if let Ok(mut addrs) = tokio::net::lookup_host(&host).await
                    && let Some(addr) = addrs.next()
                {
                    dht.add_node(crate::dht::krpc::NodeInfo {
                        id: Id20::random(),
                        addr,
                    });
                }
            }
        });
    }

    let trackers = Trackers::new(
        TrackerConfig {
            info_hash,
            peer_id,
            port: cfg.listen_port,
            num_want: 200,
            client: reqwest::Client::builder()
                .user_agent(concat!("torrra/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        },
        tiers,
        peers_tx.clone(),
    );

    let mut engine = Engine {
        name: metainfo
            .as_ref()
            .map(|i| i.name.clone())
            .unwrap_or_else(|| snapshot.lock().unwrap().name.clone()),
        info_hash,
        peer_id,
        state: if metainfo.is_some() {
            State::Verificando
        } else {
            State::BuscandoMetadata
        },
        erro: None,
        download: None,
        metadata: None,
        metadata_size: None,
        private,
        peers: HashMap::new(),
        proximo_slot: 0,
        candidatos: Candidatos::default(),
        conectando: 0,
        trackers: Arc::clone(&trackers),
        dht: dht.filter(|_| cfg.use_dht && !private),
        proximo_dht: Instant::now(),
        dht_ocupado: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        proximo_pex: Instant::now() + PEX_INTERVALO,
        pex_enviados: HashSet::new(),
        down: RateMeter::new(),
        up: RateMeter::new(),
        hist_down: History::new(120),
        hist_up: History::new(120),
        ultimo_hist: Instant::now(),
        ciclo_choke: 0,
        proximo_choke: Instant::now() + CHOKE_INTERVALO,
        otimista: None,
        check_progress: Arc::new(AtomicUsize::new(0)),
        eventos_tx,
        disco_tx,
        peers_tx,
        snapshot: Arc::clone(&snapshot),
        cfg,
    };

    engine.adicionar_candidatos(magnet_peers);

    // Com o `.torrent` em mãos já dá para preparar o disco e conferir o que
    // existe; com magnet isso espera o metadata chegar.
    if let Some(info) = metainfo {
        engine.preparar_download(info).await?;
    }
    engine.trackers.announce(Event::Started);

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(ev) = eventos_rx.recv() => engine.on_peer_event(ev).await,
            Some(ev) = disco_rx.recv() => engine.on_disk_event(ev).await,
            Some(addrs) = peers_rx.recv() => engine.adicionar_candidatos(addrs),
            cmd = cmds.recv() => match cmd {
                Some(Command::Shutdown) | None => break,
                Some(cmd) => engine.on_command(cmd).await,
            },
            _ = tick.tick() => engine.tick().await,
        }
    }

    engine.encerrar().await;
    Ok(())
}

impl Engine {
    // -- ciclo de vida -----------------------------------------------------

    /// Abre os arquivos e dispara a verificação do que já está em disco.
    async fn preparar_download(&mut self, info: Info) -> Result<()> {
        let info = Arc::new(info);
        self.name = info.name.clone();
        self.private = self.private || info.private;
        if self.private {
            self.dht = None;
        }

        let storage = Arc::new(
            Storage::open(Arc::clone(&info), &self.cfg.dest)
                .context("não consegui preparar os arquivos de destino")?,
        );

        self.state = State::Verificando;
        let progresso = Arc::clone(&self.check_progress);
        progresso.store(0, Ordering::Relaxed);

        let st = Arc::clone(&storage);
        let disco = self.disco_tx.clone();
        tokio::task::spawn_blocking(move || {
            let resultado = st.verify_all(&progresso);
            let bitfield = resultado.unwrap_or_else(|_| Bitfield::new(0));
            let _ = disco.blocking_send(DiskEvent::VerificacaoPronta(Box::new(bitfield)));
        });

        let picker = Picker::new(&info, Bitfield::new(info.num_pieces()));
        self.download = Some(Download {
            info,
            storage,
            picker,
        });
        Ok(())
    }

    async fn encerrar(&mut self) {
        for peer in self.peers.values() {
            peer.handle.send(PeerCommand::Shutdown);
        }
        self.atualizar_contadores();
        self.trackers
            .announce_stopped(Duration::from_secs(3))
            .await;
        self.trackers.shutdown();
    }

    // -- eventos de peers --------------------------------------------------

    async fn on_peer_event(&mut self, ev: PeerEvent) {
        match ev {
            PeerEvent::Connected {
                slot,
                peer_id,
                supports_dht,
                ..
            } => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    crate::log::log!("peer", "{} handshake ok", peer.addr);
                    peer.conectado_em = Instant::now();
                    // O nome do cliente sai do peer id; se o handshake de
                    // extensões trouxer um `v`, ele substitui depois.
                    peer.client = crate::client_id::nome(&peer_id);
                    peer.fala_dht = supports_dht;
                    if !std::mem::replace(&mut peer.handshake_ok, true) {
                        self.conectando = self.conectando.saturating_sub(1);
                    }
                }
            }

            PeerEvent::Extensions(slot, ext) => {
                let tamanho = ext.metadata_size;
                if let Some(peer) = self.peers.get_mut(&slot) {
                    if let Some(v) = &ext.client {
                        peer.client = v.clone();
                    }
                    // O peer diz quantos pedidos aguenta; respeitar isso evita
                    // ser desconectado por excesso.
                    if let Some(reqq) = ext.reqq {
                        peer.queue_depth = peer.queue_depth.min(reqq.max(QUEUE_MIN));
                    }
                    peer.extensions = *ext;
                }
                if self.download.is_none()
                    && let Some(tamanho) = tamanho
                {
                    self.iniciar_metadata(tamanho);
                    self.pedir_metadata(slot);
                }
            }

            PeerEvent::Bitfield(slot, bits) => {
                let Some(n) = self.num_pieces() else {
                    // Sem metadata ainda: guardamos e reinterpretamos depois.
                    if let Some(peer) = self.peers.get_mut(&slot) {
                        peer.have_all = !bits.is_empty() && bits.iter().all(|b| *b == 0xff);
                    }
                    return;
                };
                match Bitfield::from_bytes(&bits, n) {
                    Some(bf) => {
                        self.remover_disponibilidade(slot);
                        if let Some(peer) = self.peers.get_mut(&slot) {
                            peer.bitfield = bf;
                        }
                        self.aplicar_disponibilidade(slot);
                    }
                    // Bitfield com tamanho errado é peer confuso: derruba.
                    None => self.desconectar(slot, "bitfield inválido").await,
                }
            }

            PeerEvent::HaveAll(slot) => {
                self.remover_disponibilidade(slot);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.have_all = true;
                    peer.bitfield.set_all();
                }
                self.aplicar_disponibilidade(slot);
            }

            PeerEvent::HaveNone(slot) => {
                self.remover_disponibilidade(slot);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.have_all = false;
                    peer.bitfield = Bitfield::new(peer.bitfield.len());
                }
            }

            PeerEvent::Have(slot, piece) => {
                let Some(peer) = self.peers.get_mut(&slot) else {
                    return;
                };
                if peer.bitfield.is_empty() {
                    peer.pendentes.push(piece);
                    return;
                }
                if peer.bitfield.set(piece as usize)
                    && peer.contabilizado
                    && let Some(d) = &mut self.download
                {
                    d.picker.peer_has(piece);
                }
            }

            PeerEvent::Choked(slot, choked) => {
                let mut soltar = Vec::new();
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.peer_choking = choked;
                    if choked {
                        // Estrangulados, os pedidos em voo nunca chegarão.
                        soltar = std::mem::take(&mut peer.in_flight);
                    }
                }
                if !soltar.is_empty()
                    && let Some(d) = &mut self.download
                {
                    d.picker.release_peer(slot);
                }
            }

            PeerEvent::Interested(slot, interested) => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.peer_interested = interested;
                }
            }

            PeerEvent::Block {
                slot,
                request,
                data,
            } => self.on_block(slot, request, data).await,

            PeerEvent::Rejected(slot, req) => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.in_flight.retain(|r| *r != req);
                }
                if let Some(d) = &mut self.download {
                    // Devolve só este bloco ao pool, sem punir o peer.
                    d.picker.release_peer(slot);
                }
            }

            PeerEvent::BlockRequested(slot, req) => self.servir_bloco(slot, req),

            PeerEvent::Metadata(slot, msg) => self.on_metadata(slot, msg).await,

            PeerEvent::Pex(pex) => {
                if !self.private {
                    self.adicionar_candidatos(pex.added);
                }
            }

            PeerEvent::DhtPort(slot, port) => {
                // Um nó com id aleatório entra na tabela como candidato: o
                // primeiro `ping` corrige o id de verdade.
                if let (Some(dht), Some(peer)) = (&self.dht, self.peers.get(&slot))
                    && peer.fala_dht
                    && port != 0
                {
                    dht.add_node(crate::dht::krpc::NodeInfo {
                        id: Id20::random(),
                        addr: SocketAddr::new(peer.addr.ip(), port),
                    });
                }
            }

            PeerEvent::Uploaded(slot, bytes) => {
                self.up.add(bytes);
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.up.add(bytes);
                }
            }

            PeerEvent::Disconnected(slot, motivo) => {
                if let Some(p) = self.peers.get(&slot) {
                    crate::log::log!(
                        "peer",
                        "{} caiu após {:?} (handshake={}): {motivo}",
                        p.addr,
                        p.conectado_em.elapsed(),
                        p.handshake_ok
                    );
                }
                self.remover_peer(slot);
            }
        }
    }

    async fn on_block(&mut self, slot: Slot, request: BlockRequest, data: Vec<u8>) {
        let bytes = data.len() as u64;
        self.down.add(bytes);
        if let Some(peer) = self.peers.get_mut(&slot) {
            peer.down.add(bytes);
            peer.ultimo_bloco = Instant::now();
            peer.snubbed = false;
            peer.in_flight.retain(|r| {
                !(r.piece == request.piece && r.offset == request.offset)
            });
        }

        let Some(d) = &mut self.download else {
            return;
        };
        let resultado = d.picker.received(slot, request, &data);
        if !resultado.wanted {
            return;
        }

        // No endgame, quem chegou primeiro cancela os pedidos gêmeos.
        for outro in resultado.cancel_from {
            if let Some(peer) = self.peers.get_mut(&outro) {
                peer.in_flight.retain(|r| {
                    !(r.piece == request.piece && r.offset == request.offset)
                });
                peer.handle.send(PeerCommand::Cancel(request));
            }
        }

        if let Some(dados) = resultado.piece_data {
            let piece = request.piece;
            let esperado = d.info.piece_hashes[piece as usize];
            let storage = Arc::clone(&d.storage);
            let disco = self.disco_tx.clone();
            tokio::task::spawn_blocking(move || {
                // Confere antes de gravar: peça ruim nunca chega ao disco.
                let ok = Id20(Sha1::digest(&dados).into()) == esperado;
                if ok && let Err(e) = storage.write_piece(piece as usize, &dados) {
                    let _ = disco.blocking_send(DiskEvent::ErroDeEscrita(format!("{e:#}")));
                    return;
                }
                let _ = disco.blocking_send(DiskEvent::Verificado { piece, ok });
            });
        }
    }

    async fn on_disk_event(&mut self, ev: DiskEvent) {
        match ev {
            DiskEvent::VerificacaoPronta(bitfield) => {
                if let Some(d) = &mut self.download {
                    let mut picker = Picker::new(&d.info, *bitfield);
                    picker.set_mode(if self.cfg.sequential {
                        PickMode::Sequential
                    } else {
                        PickMode::Rarest
                    });
                    // A disponibilidade some junto com o picker antigo.
                    for peer in self.peers.values_mut() {
                        peer.contabilizado = false;
                    }
                    d.picker = picker;
                }
                self.state = if self.esta_completo() {
                    State::Semeando
                } else {
                    State::Baixando
                };
                // Peers que conectaram durante a verificação receberam um
                // bitfield vazio; contamos a eles o que encontramos no disco.
                let slots: Vec<Slot> = self.peers.keys().copied().collect();
                for slot in slots {
                    self.anunciar_pecas(slot);
                    self.aplicar_disponibilidade(slot);
                }
                if self.esta_completo() {
                    self.trackers.announce(Event::Completed);
                }
            }

            DiskEvent::Verificado { piece, ok } => {
                let Some(d) = &mut self.download else {
                    return;
                };
                if !ok {
                    d.picker.piece_failed(piece);
                    return;
                }
                d.picker.piece_verified(piece);
                let completo = d.picker.is_complete();

                for peer in self.peers.values() {
                    peer.handle.send(PeerCommand::Have(piece));
                }
                if completo {
                    self.ao_completar().await;
                }
            }

            DiskEvent::ErroDeEscrita(msg) => {
                self.state = State::Erro;
                self.erro = Some(msg);
            }

            DiskEvent::BlocoLido {
                slot,
                request,
                data,
            } => {
                if let Some(peer) = self.peers.get(&slot) {
                    peer.handle.send(PeerCommand::SendBlock(request, data));
                }
            }
        }
    }

    async fn ao_completar(&mut self) {
        self.state = State::Semeando;
        self.atualizar_contadores();
        self.trackers.announce(Event::Completed);

        // Peers que também têm tudo não servem mais para nada.
        let inuteis: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| p.is_seed())
            .map(|(s, _)| *s)
            .collect();
        for slot in inuteis {
            self.desconectar(slot, "ambos completos").await;
        }
        if !self.cfg.seed_after_complete {
            let todos: Vec<Slot> = self.peers.keys().copied().collect();
            for slot in todos {
                self.desconectar(slot, "download concluído").await;
            }
        }
    }

    // -- metadata (magnet) -------------------------------------------------

    fn iniciar_metadata(&mut self, tamanho: usize) {
        // 16 MiB de `info` já seria um torrent absurdo; acima disso é abuso.
        if self.metadata.is_some() || tamanho == 0 || tamanho > 16 << 20 {
            return;
        }
        self.metadata_size = Some(tamanho);
        self.metadata = Some(MetadataBuffer::new(tamanho));
    }

    fn pedir_metadata(&mut self, slot: Slot) {
        let Some(buffer) = &self.metadata else {
            return;
        };
        let faltando: Vec<usize> = buffer.missing().take(8).collect();
        if let Some(peer) = self.peers.get_mut(&slot) {
            if !peer.extensions.supports_metadata() {
                return;
            }
            peer.metadata_pedido = Some(Instant::now());
            for piece in faltando {
                peer.handle.send(PeerCommand::RequestMetadata(piece));
            }
        }
    }

    async fn on_metadata(&mut self, slot: Slot, msg: MetadataMessage) {
        match msg {
            MetadataMessage::Data { piece, total, data } => {
                self.iniciar_metadata(total);
                let Some(buffer) = &mut self.metadata else {
                    return;
                };
                if !buffer.add(piece, &data) || !buffer.is_complete() {
                    return;
                }

                let bruto = self.metadata.take().expect("acabou de existir").take();
                // O `info` recebido tem que bater com o info_hash do magnet —
                // sem essa checagem qualquer peer poderia nos entregar outro
                // torrent.
                if Id20(Sha1::digest(&bruto).into()) != self.info_hash {
                    self.metadata_size = None;
                    return;
                }
                match Info::from_raw(&bruto) {
                    Ok(info) => {
                        if let Err(e) = self.preparar_download(info).await {
                            self.state = State::Erro;
                            self.erro = Some(format!("{e:#}"));
                        }
                    }
                    Err(e) => {
                        self.state = State::Erro;
                        self.erro = Some(format!("metadata inválida: {e:#}"));
                    }
                }
            }

            // Um peer pediu nosso `info` — respondemos se já o temos.
            MetadataMessage::Request { piece } => {
                let (Some(d), Some(peer)) = (&self.download, self.peers.get(&slot)) else {
                    return;
                };
                let bruto = &d.info.raw;
                let inicio = piece * METADATA_PIECE;
                if inicio >= bruto.len() {
                    peer.handle.send(PeerCommand::SendMetadata(
                        MetadataMessage::Reject { piece },
                    ));
                    return;
                }
                let fim = (inicio + METADATA_PIECE).min(bruto.len());
                peer.handle
                    .send(PeerCommand::SendMetadata(MetadataMessage::Data {
                        piece,
                        total: bruto.len(),
                        data: bruto[inicio..fim].to_vec(),
                    }));
            }

            MetadataMessage::Reject { .. } => {
                if let Some(peer) = self.peers.get_mut(&slot) {
                    peer.metadata_pedido = None;
                }
            }
        }
    }

    // -- upload ------------------------------------------------------------

    fn servir_bloco(&mut self, slot: Slot, req: BlockRequest) {
        let Some(d) = &self.download else {
            return;
        };
        let Some(peer) = self.peers.get(&slot) else {
            return;
        };
        let valido = req.length <= BLOCK_SIZE * 8
            && (req.piece as usize) < d.info.num_pieces()
            && req.offset.saturating_add(req.length) <= d.info.piece_size(req.piece as usize);

        if peer.am_choking || !valido || !d.picker.has_piece(req.piece) {
            peer.handle.send(PeerCommand::Reject(req));
            return;
        }
        // Estourar o limite de upload agora atrasaria o download de todos.
        if let Some(limite) = self.cfg.max_upload
            && self.up.rate() > limite as f64
        {
            peer.handle.send(PeerCommand::Reject(req));
            return;
        }

        let storage = Arc::clone(&d.storage);
        let disco = self.disco_tx.clone();
        tokio::task::spawn_blocking(move || {
            match storage.read_block(req.piece as usize, req.offset, req.length) {
                Ok(data) => {
                    let _ = disco.blocking_send(DiskEvent::BlocoLido {
                        slot,
                        request: req,
                        data,
                    });
                }
                Err(_) => {
                    let _ = disco.blocking_send(DiskEvent::BlocoLido {
                        slot,
                        request: req,
                        data: Vec::new(),
                    });
                }
            }
        });
    }

    // -- laço periódico ----------------------------------------------------

    async fn tick(&mut self) {
        if self.state != State::Pausado {
            self.conectar_candidatos().await;
            self.liberar_travados();
            self.atualizar_interesse();
            self.pedir_blocos();
            self.cobrar_metadata();

            if Instant::now() >= self.proximo_pex {
                self.proximo_pex = Instant::now() + PEX_INTERVALO;
                self.enviar_pex();
            }
            if Instant::now() >= self.proximo_choke {
                self.rodar_choking();
                self.proximo_choke = Instant::now() + CHOKE_INTERVALO;
            }
            if let Some(dht) = self.dht.clone()
                && Instant::now() >= self.proximo_dht
                && !self.dht_ocupado.load(Ordering::Relaxed)
            {
                // Enquanto o DHT está se formando, ou enquanto faltam peers,
                // insistir de minuto em minuto. Esperar o intervalo cheio
                // desperdiçaria os primeiros 15 minutos de download, que é
                // justamente quando o tracker ainda deu pouca coisa.
                if !dht.is_ready() {
                    // Buscar com a tabela vazia não acha nada e só queima o
                    // intervalo; esperamos o bootstrap dar sinal de vida.
                    self.proximo_dht = Instant::now() + Duration::from_secs(3);
                } else {
                    let carente = self.peers.len() < self.cfg.max_peers / 4;
                    self.proximo_dht = Instant::now()
                        + if carente { DHT_RETENTATIVA } else { DHT_INTERVALO };
                    self.buscar_no_dht(dht);
                }
            }
        }

        self.atualizar_contadores();
        if self.ultimo_hist.elapsed() >= Duration::from_secs(1) {
            if crate::log::ativo("estado") {
                let livres = self.peers.values().filter(|p| !p.peer_choking).count();
                let interessados = self.peers.values().filter(|p| p.am_interested).count();
                let voo: usize = self.peers.values().map(|p| p.in_flight.len()).sum();
                let fila: usize = self.peers.values().map(|p| p.queue_depth).sum();
                let com_bf = self.peers.values().filter(|p| p.bitfield.count() > 0).count();
                crate::log::log!(
                    "estado",
                    "{} peers · {} nos liberaram · {} nos interessam · {} com bitfield · {} blocos em voo · fila somada {}",
                    self.peers.len(), livres, interessados, com_bf, voo, fila
                );
            }
            self.ultimo_hist = Instant::now();
            let (d, u) = (self.down.rate(), self.up.rate());
            self.hist_down.push(d);
            self.hist_up.push(u);
        }
        self.publicar_snapshot();
    }

    /// Blocos pedidos que nunca chegaram voltam ao pool, e o peer culpado é
    /// marcado como "snubbed" para perder prioridade no choking.
    fn liberar_travados(&mut self) {
        let Some(d) = &mut self.download else {
            return;
        };
        d.picker.sweep_timeouts(BLOCO_TIMEOUT);

        for (slot, peer) in self.peers.iter_mut() {
            if !peer.in_flight.is_empty() && peer.ultimo_bloco.elapsed() > SNUB_TIMEOUT {
                peer.snubbed = true;
                peer.in_flight.clear();
                peer.queue_depth = QUEUE_MIN;
                d.picker.release_peer(*slot);
            }
        }
    }

    /// Diz a cada peer se ele tem algo que queremos.
    fn atualizar_interesse(&mut self) {
        let Some(d) = &self.download else {
            return;
        };
        let nosso = d.picker.have();
        for peer in self.peers.values_mut() {
            let quer = peer.have_all || nosso.has_anything_we_lack(&peer.bitfield);
            if quer != peer.am_interested {
                peer.am_interested = quer;
                peer.handle.send(PeerCommand::Interested(quer));
            }
        }
    }

    /// Reabastece a fila de pedidos de cada peer liberado.
    fn pedir_blocos(&mut self) {
        let Some(d) = &mut self.download else {
            return;
        };
        if d.picker.is_complete() {
            return;
        }
        // Estourar o limite de download: parar de pedir é o freio mais direto.
        if let Some(limite) = self.cfg.max_download
            && self.down.rate() > limite as f64
        {
            return;
        }

        let mut slots: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| !p.peer_choking && p.am_interested)
            .map(|(s, _)| *s)
            .collect();
        // Os mais rápidos escolhem primeiro: as peças raras vão para quem as
        // entrega antes.
        slots.sort_unstable_by(|a, b| {
            let taxa = |s: &Slot| self.peers.get(s).map(|p| p.down.total()).unwrap_or(0);
            taxa(b).cmp(&taxa(a))
        });

        let mut pedidos = Vec::new();
        for slot in slots {
            let Some(peer) = self.peers.get_mut(&slot) else {
                continue;
            };
            peer.queue_depth = profundidade_ideal(peer.down.rate(), peer.queue_depth);
            let faltam = peer.queue_depth.saturating_sub(peer.in_flight.len());
            if faltam == 0 {
                continue;
            }

            pedidos.clear();
            d.picker.pick(slot, &peer.bitfield, faltam, &mut pedidos);
            if pedidos.is_empty() {
                continue;
            }
            peer.in_flight.extend(pedidos.iter().copied());
            peer.handle.send(PeerCommand::Request(pedidos.clone()));
        }
    }

    /// Conta aos peers quem entrou e quem saiu desde a última troca.
    ///
    /// Trocar peers direto entre clientes é o que mantém um enxame vivo quando
    /// o tracker é avarento — e a maior parte deles é.
    fn enviar_pex(&mut self) {
        if self.private {
            return;
        }
        let atuais: HashSet<SocketAddr> = self
            .peers
            .values()
            .filter(|p| p.handshake_ok && p.discamos)
            .map(|p| p.addr)
            .collect();

        let msg = PexMessage {
            added: atuais
                .difference(&self.pex_enviados)
                .copied()
                .take(PEX_MAX)
                .collect(),
            dropped: self
                .pex_enviados
                .difference(&atuais)
                .copied()
                .take(PEX_MAX)
                .collect(),
        };
        if msg.added.is_empty() && msg.dropped.is_empty() {
            return;
        }
        for peer in self.peers.values().filter(|p| p.extensions.supports_pex()) {
            peer.handle.send(PeerCommand::SendPex(msg.clone()));
        }
        self.pex_enviados = atuais;
    }

    fn cobrar_metadata(&mut self) {
        if self.download.is_some() || self.metadata.is_none() {
            return;
        }
        let alvos: Vec<Slot> = self
            .peers
            .iter()
            .filter(|(_, p)| {
                p.extensions.supports_metadata()
                    && p.metadata_pedido
                        .is_none_or(|t| t.elapsed() > Duration::from_secs(10))
            })
            .map(|(s, _)| *s)
            .take(4)
            .collect();
        for slot in alvos {
            self.pedir_metadata(slot);
        }
    }

    /// Tit-for-tat: liberamos quem nos entrega mais, e mais um peer sorteado
    /// para dar chance a quem ainda não provou nada.
    fn rodar_choking(&mut self) {
        self.ciclo_choke += 1;
        let semeando = self.esta_completo();

        let mut ranking: Vec<(Slot, f64)> = self
            .peers
            .iter_mut()
            .filter(|(_, p)| p.peer_interested)
            .map(|(slot, p)| {
                // Baixando, o que vale é o quanto o peer nos dá. Semeando, não
                // há reciprocidade a medir: premiamos quem consome mais rápido.
                let pontos = if semeando { p.up.rate() } else { p.down.rate() };
                let pontos = if p.snubbed { 0.0 } else { pontos };
                (*slot, pontos)
            })
            .collect();
        ranking.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));

        let mut liberados: HashSet<Slot> = ranking
            .iter()
            .take(self.cfg.upload_slots.saturating_sub(1))
            .map(|(s, _)| *s)
            .collect();

        // Unchoke otimista, trocado a cada ~30 s.
        if self.ciclo_choke % OPTIMISTIC_CICLOS == 1 || self.otimista.is_none_or(|s| !self.peers.contains_key(&s)) {
            let candidatos: Vec<Slot> = self
                .peers
                .iter()
                .filter(|(s, p)| p.peer_interested && !liberados.contains(s))
                .map(|(s, _)| *s)
                .collect();
            self.otimista = escolher_otimista(&candidatos, self.ciclo_choke);
        }
        if let Some(slot) = self.otimista {
            liberados.insert(slot);
        }

        for (slot, peer) in self.peers.iter_mut() {
            let deve_estrangular = !liberados.contains(slot);
            if deve_estrangular != peer.am_choking {
                peer.am_choking = deve_estrangular;
                peer.handle.send(PeerCommand::Choke(deve_estrangular));
            }
        }
    }

    /// Procura peers no DHT. Os resultados entram pelo mesmo canal dos
    /// trackers, então chegam ao engine sem tratamento especial.
    fn buscar_no_dht(&self, dht: Arc<Dht>) {
        let info_hash = self.info_hash;
        let porta = self.cfg.listen_port;
        let temos_tudo = self.esta_completo();
        let peers_tx = self.peers_tx.clone();
        let ocupado = Arc::clone(&self.dht_ocupado);

        ocupado.store(true, Ordering::Relaxed);
        tokio::spawn(async move {
            dht.get_peers(info_hash, Some(peers_tx)).await;
            // Só anunciamos quando temos algo a oferecer; um leecher que se
            // anuncia atrai conexões que ele não consegue atender.
            if temos_tudo {
                dht.announce_peer(info_hash, porta).await;
            }
            ocupado.store(false, Ordering::Relaxed);
        });
    }

    // -- conexões ----------------------------------------------------------

    fn adicionar_candidatos(&mut self, addrs: Vec<SocketAddr>) {
        crate::log::log!("peers", "descobertos {} endereços", addrs.len());
        for addr in addrs {
            self.candidatos.oferecer(addr);
        }
    }

    async fn conectar_candidatos(&mut self) {
        let vivos = self.peers.len().saturating_sub(self.conectando);
        while vivos + self.conectando < self.cfg.max_peers && self.conectando < MAX_CONECTANDO {
            let Some(addr) = self.candidatos.proximo() else {
                break;
            };
            if self.peers.values().any(|p| p.addr == addr) {
                continue;
            }

            let slot = self.novo_slot();
            let handle = peer::spawn_outgoing(
                slot,
                addr,
                self.peer_config(),
                self.eventos_tx.clone(),
                CONNECT_TIMEOUT,
            )
            .await;
            self.registrar_peer(slot, addr, handle, true);
            self.conectando += 1;
        }
    }

    fn peer_config(&self) -> peer::PeerConfig {
        peer::PeerConfig {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            listen_port: self.cfg.listen_port,
            metadata_size: self
                .download
                .as_ref()
                .map(|d| d.info.raw.len())
                .or(self.metadata_size),
            allow_pex: !self.private,
            dht: self.dht.is_some(),
            initial_bitfield: self
                .download
                .as_ref()
                .filter(|d| !d.picker.have().is_empty())
                .map(|d| d.picker.have().as_bytes().to_vec()),
        }
    }

    fn novo_slot(&mut self) -> Slot {
        self.proximo_slot += 1;
        self.proximo_slot
    }

    fn registrar_peer(&mut self, slot: Slot, addr: SocketAddr, handle: PeerHandle, discamos: bool) {
        let n = self.num_pieces().unwrap_or(0);
        self.peers.insert(
            slot,
            PeerState {
                handle,
                addr,
                client: String::new(),
                bitfield: Bitfield::new(n),
                have_all: false,
                pendentes: Vec::new(),
                contabilizado: false,
                handshake_ok: false,
                discamos,
                am_choking: true,
                am_interested: false,
                peer_choking: true,
                peer_interested: false,
                in_flight: Vec::new(),
                queue_depth: QUEUE_MIN * 2,
                down: RateMeter::new(),
                up: RateMeter::new(),
                extensions: ExtensionHandshake::default(),
                metadata_pedido: None,
                fala_dht: false,
                ultimo_bloco: Instant::now(),
                conectado_em: Instant::now(),
                snubbed: false,
            },
        );
    }

    fn remover_peer(&mut self, slot: Slot) {
        self.remover_disponibilidade(slot);
        // Um peer que morre antes do handshake precisa devolver a vaga de
        // conexão que reservou — sem isso o contador satura e o engine para de
        // discar para sempre, com a fila de candidatos cheia.
        if let Some(peer) = self.peers.remove(&slot) {
            if !peer.handshake_ok {
                self.conectando = self.conectando.saturating_sub(1);
            }
            // Quem chegou a conversar merece uma nova chance mais cedo do que
            // um endereço que nem respondeu ao handshake.
            if peer.discamos {
                let espera = if peer.handshake_ok {
                    REENTRADA_BOA
                } else {
                    REENTRADA_RUIM
                };
                self.candidatos.devolver(peer.addr, espera);
            }
            if let Some(d) = &mut self.download {
                d.picker.release_peer(slot);
            }
        }
        if self.otimista == Some(slot) {
            self.otimista = None;
        }
    }

    async fn desconectar(&mut self, slot: Slot, _motivo: &str) {
        if let Some(peer) = self.peers.get(&slot) {
            peer.handle.send(PeerCommand::Shutdown);
        }
        self.remover_peer(slot);
    }

    /// Conta a um peer já conectado as peças que descobrimos depois do
    /// handshake — o `bitfield` inicial já foi (ou não) enviado pela task dele.
    fn anunciar_pecas(&self, slot: Slot) {
        let (Some(d), Some(peer)) = (&self.download, self.peers.get(&slot)) else {
            return;
        };
        for piece in d.picker.have().iter_set() {
            peer.handle.send(PeerCommand::Have(piece as u32));
        }
    }

    fn aplicar_disponibilidade(&mut self, slot: Slot) {
        let Some(d) = &mut self.download else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&slot) else {
            return;
        };
        if peer.bitfield.is_empty() {
            peer.bitfield = Bitfield::new(d.info.num_pieces());
            if peer.have_all {
                peer.bitfield.set_all();
            }
            for piece in std::mem::take(&mut peer.pendentes) {
                peer.bitfield.set(piece as usize);
            }
        }
        if !peer.contabilizado {
            d.picker.peer_added(&peer.bitfield);
            peer.contabilizado = true;
        }
    }

    fn remover_disponibilidade(&mut self, slot: Slot) {
        let Some(d) = &mut self.download else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&slot) else {
            return;
        };
        if peer.contabilizado {
            d.picker.peer_removed(&peer.bitfield);
            peer.contabilizado = false;
        }
    }

    // -- comandos ----------------------------------------------------------

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Pause => {
                if self.state == State::Pausado {
                    return;
                }
                self.state = State::Pausado;
                let todos: Vec<Slot> = self.peers.keys().copied().collect();
                for slot in todos {
                    self.desconectar(slot, "pausado").await;
                }
                // Pausar não pode custar o enxame: os endereços saem da
                // quarentena para que retomar volte a discar na hora.
                self.candidatos.liberar_tudo();
                self.trackers.announce(Event::Stopped);
            }
            Command::Resume => {
                if self.state != State::Pausado {
                    return;
                }
                self.state = if self.esta_completo() {
                    State::Semeando
                } else {
                    State::Baixando
                };
                self.candidatos.reenfileirar();
                self.proximo_dht = Instant::now();
                self.trackers.announce(Event::Started);
            }
            Command::SetMode(modo) => {
                self.cfg.sequential = modo == PickMode::Sequential;
                if let Some(d) = &mut self.download {
                    d.picker.set_mode(modo);
                }
            }
            Command::SetFilePriority(indice, prio) => {
                if let Some(d) = &mut self.download {
                    let info = Arc::clone(&d.info);
                    d.picker.set_file_priority(&info, indice, prio);
                }
            }
            Command::Incoming(dados) => {
                let (socket, addr, handshake) = *dados;
                if self.peers.len() >= self.cfg.max_peers {
                    return;
                }
                let slot = self.novo_slot();
                let handle = peer::spawn_incoming(
                    slot,
                    socket,
                    addr,
                    handshake,
                    self.peer_config(),
                    self.eventos_tx.clone(),
                )
                .await;
                self.registrar_peer(slot, addr, handle, false);
            }
            Command::Recheck => {
                if let Some(d) = &self.download {
                    let info = Arc::clone(&d.info);
                    let _ = self.preparar_download((*info).clone()).await;
                }
            }
            Command::Shutdown => {}
        }
    }

    // -- consultas ---------------------------------------------------------

    fn num_pieces(&self) -> Option<usize> {
        self.download.as_ref().map(|d| d.info.num_pieces())
    }

    fn esta_completo(&self) -> bool {
        self.download
            .as_ref()
            .is_some_and(|d| d.picker.is_complete())
    }

    fn atualizar_contadores(&self) {
        let falta = match &self.download {
            Some(d) => d.picker.remaining(),
            None => 0,
        };
        // `downloaded` no announce é o que veio da rede nesta sessão, não o que
        // está em disco — o tracker usa isso para calcular a razão.
        self.trackers
            .counters()
            .set(self.up.total(), self.down.total(), falta);
    }

    fn publicar_snapshot(&mut self) {
        let taxa_down = self.down.rate();
        let taxa_up = self.up.rate();

        let (have, total_pieces, pieces, partials, baixado, falta, total, endgame) =
            match &self.download {
                Some(d) => (
                    d.picker.have().count(),
                    d.info.num_pieces(),
                    d.picker.have().as_bytes().to_vec(),
                    d.picker.partial_pieces(),
                    d.picker.downloaded(),
                    d.picker.remaining(),
                    d.info.total_length,
                    d.picker.in_endgame(),
                ),
                None => (0, 0, Vec::new(), Vec::new(), 0, 0, 0, false),
            };

        let arquivos = match &self.download {
            Some(d) => d
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| FileInfo {
                    path: f.path.clone(),
                    length: f.length,
                    downloaded: bytes_do_arquivo(d, f.offset, f.length),
                    priority: d.picker.file_priority(i),
                })
                .collect(),
            None => Vec::new(),
        };

        // Só peers com handshake concluído contam: os demais ainda estão
        // discando, e mostrá-los faria a lista parecer cheia enquanto o
        // download não sai do lugar.
        let mut peers: Vec<PeerInfo> = self
            .peers
            .values_mut()
            .filter(|p| p.handshake_ok)
            .map(|p| PeerInfo {
                addr: p.addr,
                client: if p.client.is_empty() {
                    "—".to_string()
                } else {
                    p.client.clone()
                },
                progress: p.progresso(),
                download_rate: p.down.rate(),
                upload_rate: p.up.rate(),
                choking_us: p.peer_choking,
                interested_in_us: p.peer_interested,
                we_choke: p.am_choking,
                in_flight: p.in_flight.len(),
                snubbed: p.snubbed,
            })
            .collect();
        peers.sort_unstable_by(|a, b| b.download_rate.total_cmp(&a.download_rate));

        let estatisticas_dht = self
            .dht
            .as_ref()
            .map(|d| d.stats())
            .unwrap_or_default();
        let status_trackers = self.trackers.status();
        let (seeds, leechers) = status_trackers.iter().fold((0, 0), |(s, l), t| {
            (s.max(t.seeders), l.max(t.leechers))
        });

        let eta = if taxa_down > 1.0 && falta > 0 {
            Some((falta as f64 / taxa_down) as u64)
        } else if falta == 0 {
            Some(0)
        } else {
            None
        };

        let check = if self.state == State::Verificando && total_pieces > 0 {
            self.check_progress.load(Ordering::Relaxed) as f64 / total_pieces as f64
        } else {
            0.0
        };

        *self.snapshot.lock().unwrap() = Snapshot {
            name: self.name.clone(),
            info_hash: self.info_hash.to_hex(),
            state_label: self.state.label().to_string(),
            error: self.erro.clone(),
            have_pieces: have,
            total_pieces,
            pieces,
            partials,
            downloaded: baixado,
            uploaded: self.up.total(),
            wanted: baixado + falta,
            total_size: total,
            download_rate: taxa_down,
            upload_rate: taxa_up,
            eta,
            connected: peers.len(),
            candidates: self.candidatos.len(),
            peers,
            seeds_in_swarm: seeds,
            peers_in_swarm: leechers,
            files: arquivos,
            trackers: status_trackers,
            dht_nodes: estatisticas_dht.nodes,
            dht_good: estatisticas_dht.good_nodes,
            dht_queries: estatisticas_dht.queries_sent,
            listen_port: self.cfg.listen_port,
            down_history: self.hist_down.iter().collect(),
            up_history: self.hist_up.iter().collect(),
            sequential: self.cfg.sequential,
            endgame,
            check_progress: check,
        };
    }
}

/// Quanto de um arquivo já está em disco, contando só as peças completas.
fn bytes_do_arquivo(d: &Download, offset: u64, length: u64) -> u64 {
    if length == 0 {
        return 0;
    }
    let piece_len = d.info.piece_length as u64;
    let primeira = offset / piece_len;
    let ultima = (offset + length - 1) / piece_len;
    let mut total = 0;
    for piece in primeira..=ultima {
        if d.picker.has_piece(piece as u32) {
            let inicio = (piece * piece_len).max(offset);
            let fim = ((piece + 1) * piece_len).min(offset + length);
            total += fim.saturating_sub(inicio);
        }
    }
    total
}

/// Quantos pedidos manter em voo para um peer que entrega a `taxa` bytes/s.
///
/// A ideia é cobrir o tempo de ida e volta: com pouca coisa em voo o peer fica
/// ocioso entre respostas, e com coisa demais os pedidos envelhecem na fila
/// dele — e ainda perdemos a chance de pedir a peça a quem está mais rápido.
fn profundidade_ideal(taxa: f64, atual: usize) -> usize {
    // ~2 segundos de dados em trânsito.
    let alvo = ((taxa * 2.0) / BLOCK_SIZE as f64).ceil() as usize;
    let alvo = alvo.clamp(QUEUE_MIN, QUEUE_MAX);
    // Subir devagar e descer rápido evita oscilação.
    if alvo > atual {
        (atual + 2).min(alvo)
    } else {
        alvo
    }
}

/// Escolhe o peer do unchoke otimista de forma rotativa.
fn escolher_otimista(candidatos: &[Slot], ciclo: u32) -> Option<Slot> {
    if candidatos.is_empty() {
        return None;
    }
    Some(candidatos[(ciclo as usize) % candidatos.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profundidade_acompanha_a_velocidade() {
        // Peer lento fica no mínimo.
        assert_eq!(profundidade_ideal(0.0, QUEUE_MIN), QUEUE_MIN);
        // 10 MiB/s ≈ 640 blocos/s, dois segundos = 1280 → limitado ao teto,
        // mas subindo de dois em dois.
        assert_eq!(profundidade_ideal(10.0 * 1024.0 * 1024.0, 10), 12);
        // Queda de velocidade encolhe a fila de uma vez.
        assert_eq!(profundidade_ideal(0.0, 200), QUEUE_MIN);
        assert!(profundidade_ideal(f64::MAX, QUEUE_MAX) <= QUEUE_MAX);
    }

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 6881))
    }

    #[test]
    fn fila_nao_aceita_duplicatas_nem_lixo() {
        let mut c = Candidatos::default();
        assert!(c.oferecer(addr(1)));
        assert!(!c.oferecer(addr(1)), "endereço já na fila não repete");
        assert!(!c.oferecer("127.0.0.1:6881".parse().unwrap()), "loopback");
        assert!(!c.oferecer("1.2.3.4:0".parse().unwrap()), "porta zero");
        assert_eq!(c.len(), 1);

        assert_eq!(c.proximo(), Some(addr(1)));
        assert_eq!(c.proximo(), None);
        // Depois de sair da fila o endereço continua reservado: ele está
        // conectado agora.
        assert!(!c.oferecer(addr(1)));
    }

    #[test]
    fn peer_que_cai_volta_depois_da_quarentena() {
        let mut c = Candidatos::default();
        c.oferecer(addr(7));
        c.proximo();

        // Ainda em quarentena.
        c.devolver(addr(7), Duration::from_secs(60));
        assert!(!c.oferecer(addr(7)));

        // Quarentena vencida: pode tentar de novo.
        c.devolver(addr(7), Duration::ZERO);
        assert!(c.oferecer(addr(7)));
        assert_eq!(c.len(), 1);
    }

    /// O ciclo que quebrava: pausar desconectava todo mundo e os endereços
    /// ficavam banidos, então retomar não tinha ninguém para discar.
    #[test]
    fn pausar_e_retomar_recupera_os_peers() {
        let mut c = Candidatos::default();
        for i in 1..=5 {
            c.oferecer(addr(i));
        }
        // Todos saem da fila para conectar.
        while c.proximo().is_some() {}
        assert_eq!(c.len(), 0);

        // Pausa: desconecta todos e libera os endereços.
        c.liberar_tudo();
        assert_eq!(c.len(), 0, "a fila fica vazia enquanto pausado");

        // Retoma: os cinco voltam para a fila.
        c.reenfileirar();
        assert_eq!(c.len(), 5, "os peers conhecidos voltam ao retomar");

        // E um announce do tracker com os mesmos endereços não duplica.
        for i in 1..=5 {
            assert!(!c.oferecer(addr(i)));
        }
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn fila_tem_teto() {
        let mut c = Candidatos::default();
        for i in 0..(MAX_CANDIDATOS + 50) {
            let ip = (i / 250) as u8;
            let porta = 1024 + (i % 250) as u16;
            c.oferecer(SocketAddr::from(([10, 1, ip, 1], porta)));
        }
        assert_eq!(c.len(), MAX_CANDIDATOS, "a fila não cresce sem limite");
    }

    #[test]
    fn otimista_gira_entre_os_candidatos() {
        let candidatos = vec![1, 2, 3];
        let escolhidos: Vec<Slot> = (0..6)
            .map(|c| escolher_otimista(&candidatos, c).unwrap())
            .collect();
        assert_eq!(escolhidos, vec![1, 2, 3, 1, 2, 3]);
        assert_eq!(escolher_otimista(&[], 0), None);
    }
}
