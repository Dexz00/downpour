//! Conexão com um peer.
//!
//! Cada peer é uma task independente. Ela não decide nada sobre o download —
//! só traduz o protocolo em eventos para o engine e executa os comandos que
//! recebe de volta. Toda a política (o que pedir, quem estrangular) mora no
//! engine, que é quem enxerga o enxame inteiro.

pub mod extension;
pub mod message;

use crate::id::Id20;
use crate::picker::BlockRequest;
use anyhow::{Context, Result, bail};
use extension::{ExtensionHandshake, MetadataMessage, PexMessage};
use message::{Handshake, Message, read_message, reserved};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

/// Quanto esperamos por um handshake antes de desistir do peer.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(12);
/// Silêncio absoluto por esse tempo significa conexão morta.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// Intervalo de keep-alive; o padrão de fato é 2 minutos.
const KEEPALIVE: Duration = Duration::from_secs(100);
/// Quantos pedidos de upload enfileiramos de um mesmo peer antes de cortá-lo.
const MAX_INCOMING_REQUESTS: usize = 500;
/// Maior bloco que aceitamos servir.
const MAX_SERVE_LENGTH: u32 = 128 * 1024;

/// Identificador local do peer dentro do torrent (índice de slot).
pub type Slot = u32;

#[derive(Debug)]
pub enum PeerCommand {
    /// Pedidos a enviar, em lote (pipelining).
    Request(Vec<BlockRequest>),
    Cancel(BlockRequest),
    Have(u32),
    /// `true` = estrangular o peer (parar de enviar dados a ele).
    Choke(bool),
    Interested(bool),
    /// Dados já lidos do disco, prontos para irem para o peer.
    SendBlock(BlockRequest, Vec<u8>),
    /// Recusa explícita de um pedido (fast extension).
    Reject(BlockRequest),
    RequestMetadata(usize),
    SendMetadata(MetadataMessage),
    SendPex(PexMessage),
    Shutdown,
}

#[derive(Debug)]
pub enum PeerEvent {
    Connected {
        slot: Slot,
        peer_id: Id20,
        supports_dht: bool,
    },
    /// Handshake de extensões do peer — chega logo depois do `Connected`, e
    /// pode ser reenviado a qualquer momento para ligar/desligar extensões.
    Extensions(Slot, Box<ExtensionHandshake>),
    /// Bitfield cru: em um magnet ainda não sabemos quantas peças existem.
    Bitfield(Slot, Vec<u8>),
    HaveAll(Slot),
    HaveNone(Slot),
    Have(Slot, u32),
    /// O peer nos estrangulou (`true`) ou liberou (`false`).
    Choked(Slot, bool),
    Interested(Slot, bool),
    Block {
        slot: Slot,
        request: BlockRequest,
        data: Vec<u8>,
    },
    /// O peer recusou um pedido nosso (fast extension).
    Rejected(Slot, BlockRequest),
    /// O peer quer um bloco nosso.
    BlockRequested(Slot, BlockRequest),
    Metadata(Slot, MetadataMessage),
    Pex(PexMessage),
    /// Porta DHT anunciada pelo peer.
    DhtPort(Slot, u16),
    /// Bytes enviados a este peer desde o último aviso.
    Uploaded(Slot, u64),
    Disconnected(Slot, String),
}

#[derive(Clone)]
pub struct PeerConfig {
    pub info_hash: Id20,
    pub peer_id: Id20,
    pub listen_port: u16,
    /// Tamanho do `info`, quando já o temos (anunciado via ut_metadata).
    pub metadata_size: Option<usize>,
    /// Torrent privado desliga PEX e DHT.
    pub allow_pex: bool,
    pub dht: bool,
    /// Peças que já temos. O protocolo só aceita `bitfield` como primeira
    /// mensagem depois do handshake, então ele precisa vir pronto daqui — o
    /// que descobrirmos depois vai por `have`.
    pub initial_bitfield: Option<Vec<u8>>,
}

/// Alça do engine para falar com a task de um peer.
pub struct PeerHandle {
    tx: mpsc::Sender<PeerCommand>,
}

impl PeerHandle {
    /// Enfileira um comando. Devolve `false` se a fila do peer estourou ou a
    /// conexão morreu — em ambos os casos o comando é descartável.
    pub fn send(&self, cmd: PeerCommand) -> bool {
        self.tx.try_send(cmd).is_ok()
    }
}

/// Conecta, faz o handshake e roda o peer até a conexão morrer.
pub async fn spawn_outgoing(
    slot: Slot,
    addr: SocketAddr,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
    connect_timeout: Duration,
) -> PeerHandle {
    let (tx, rx) = mpsc::channel(256);
    let handle = PeerHandle { tx };
    tokio::spawn(async move {
        let motivo = match timeout(connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(socket)) => run(slot, socket, cfg, events.clone(), rx, None)
                .await
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "encerrado".to_string()),
            Ok(Err(e)) => format!("conexão falhou: {e}"),
            Err(_) => "tempo esgotado ao conectar".to_string(),
        };
        let _ = events.send(PeerEvent::Disconnected(slot, motivo)).await;
    });
    handle
}

/// Assume uma conexão que chegou até nós, cujo handshake o listener já leu
/// (é ele quem descobre a qual torrent a conexão pertence).
pub async fn spawn_incoming(
    slot: Slot,
    socket: TcpStream,
    addr: SocketAddr,
    handshake: Handshake,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
) -> PeerHandle {
    let (tx, rx) = mpsc::channel(256);
    let handle = PeerHandle { tx };
    let _ = addr;
    tokio::spawn(async move {
        let motivo = run(slot, socket, cfg, events.clone(), rx, Some(handshake))
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "encerrado".to_string());
        let _ = events.send(PeerEvent::Disconnected(slot, motivo)).await;
    });
    handle
}

/// Lê o handshake de uma conexão recebida antes de saber a qual torrent ela
/// pertence. Devolve o handshake e o socket, já sem consumir mais nada.
pub async fn read_incoming_handshake(socket: &mut TcpStream) -> Result<Handshake> {
    let mut buf = [0u8; 68];
    timeout(HANDSHAKE_TIMEOUT, socket.read_exact(&mut buf))
        .await
        .context("tempo esgotado esperando handshake")??;
    Handshake::from_bytes(&buf)
}

async fn run(
    slot: Slot,
    socket: TcpStream,
    cfg: PeerConfig,
    events: mpsc::Sender<PeerEvent>,
    mut cmds: mpsc::Receiver<PeerCommand>,
    handshake_lido: Option<Handshake>,
) -> Result<()> {
    // Latência importa mais que throughput agregado aqui: sem isso, um `request`
    // pequeno pode ficar preso no buffer do Nagle esperando companhia.
    socket.set_nodelay(true).ok();

    let (leitura, escrita) = socket.into_split();
    let mut reader = BufReader::with_capacity(64 * 1024, leitura);
    let mut writer = BufWriter::with_capacity(64 * 1024, escrita);

    // -- handshake ---------------------------------------------------------
    let nosso = Handshake::new(cfg.info_hash, cfg.peer_id, cfg.dht);
    writer.write_all(&nosso.to_bytes()).await?;
    writer.flush().await?;

    let deles = match handshake_lido {
        Some(hs) => hs,
        None => {
            let mut buf = [0u8; 68];
            timeout(HANDSHAKE_TIMEOUT, reader.read_exact(&mut buf))
                .await
                .context("tempo esgotado esperando handshake")??;
            Handshake::from_bytes(&buf)?
        }
    };
    if deles.info_hash != cfg.info_hash {
        bail!("peer respondeu com outro info_hash");
    }
    if deles.peer_id == cfg.peer_id {
        bail!("conexão com nós mesmos");
    }

    let suporta_ext = deles.supports(reserved::EXTENSION);
    let suporta_dht = deles.supports(reserved::DHT);

    let mut extensions = ExtensionHandshake::default();
    if suporta_ext {
        let nosso_ext =
            ExtensionHandshake::ours(cfg.metadata_size, cfg.allow_pex, cfg.listen_port);
        write_msg(
            &mut writer,
            &Message::Extended {
                id: 0,
                payload: nosso_ext.encode(),
            },
        )
        .await?;
    }
    if let Some(bits) = &cfg.initial_bitfield
        && bits.iter().any(|b| *b != 0)
    {
        write_msg(&mut writer, &Message::Bitfield(bits.clone())).await?;
    }
    if cfg.dht {
        write_msg(&mut writer, &Message::Port(cfg.listen_port)).await?;
    }
    writer.flush().await?;

    events
        .send(PeerEvent::Connected {
            slot,
            peer_id: deles.peer_id,
            supports_dht: suporta_dht,
        })
        .await
        .ok();

    // -- laço principal ----------------------------------------------------
    //
    // A leitura roda em uma task própria, e não dentro do `select!`, porque
    // `read_message` não é cancel-safe: ela consome os 4 bytes de tamanho antes
    // do corpo. Cancelada entre os dois, esses bytes somem do buffer e a
    // mensagem seguinte é lida a partir do corpo — o stream dessincroniza e a
    // conexão morre anunciando "mensagem de 1,6 GB". Receber de um canal, ao
    // contrário, é cancel-safe.
    let (msgs_tx, mut msgs_rx) = mpsc::channel::<Result<Message>>(512);
    let leitor = tokio::spawn(async move {
        loop {
            let resultado = match timeout(IDLE_TIMEOUT, read_message(&mut reader)).await {
                Ok(Ok(msg)) => Ok(msg),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!("peer ficou mudo")),
            };
            let acabou = resultado.is_err();
            if msgs_tx.send(resultado).await.is_err() || acabou {
                return;
            }
        }
    });
    let _leitor = AbortaAoSair(leitor);

    let mut keepalive = tokio::time::interval_at(Instant::now() + KEEPALIVE, KEEPALIVE);
    let mut pedidos_recebidos = 0usize;
    let mut enviados = 0u64;
    let mut ultimo_relato = Instant::now();

    loop {
        tokio::select! {
            // Prioriza drenar comandos: pedidos parados são banda ociosa.
            biased;

            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                if matches!(cmd, PeerCommand::Shutdown) {
                    return Ok(());
                }
                let bytes = apply_command(&mut writer, cmd, &extensions, &mut pedidos_recebidos).await?;
                enviados += bytes;

                // Esvazia o resto da fila antes de dar flush: um flush por
                // lote em vez de um por mensagem.
                while let Ok(cmd) = cmds.try_recv() {
                    if matches!(cmd, PeerCommand::Shutdown) {
                        writer.flush().await?;
                        return Ok(());
                    }
                    enviados += apply_command(&mut writer, cmd, &extensions, &mut pedidos_recebidos).await?;
                }
                writer.flush().await?;

                if enviados > 0 && ultimo_relato.elapsed() > Duration::from_millis(500) {
                    events.send(PeerEvent::Uploaded(slot, std::mem::take(&mut enviados))).await.ok();
                    ultimo_relato = Instant::now();
                }
            }

            msg = msgs_rx.recv() => {
                // `None` significa que a task de leitura encerrou.
                let Some(msg) = msg else { return Ok(()) };
                let msg = msg?;
                if let Some(evento) = handle_message(slot, msg, &mut extensions, &mut pedidos_recebidos)?
                    && events.send(evento).await.is_err() {
                        return Ok(());
                    }
            }

            _ = keepalive.tick() => {
                write_msg(&mut writer, &Message::KeepAlive).await?;
                writer.flush().await?;
            }
        }
    }
}

/// Garante que a task de leitura morra junto com o peer, em vez de ficar
/// pendurada em `read_message` até o timeout de inatividade.
struct AbortaAoSair(#[allow(dead_code)] tokio::task::JoinHandle<()>);

impl Drop for AbortaAoSair {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn write_msg<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, msg: &Message) -> Result<()> {
    message::write_message(writer, msg).await
}

/// Executa um comando do engine; devolve quantos bytes de *payload* foram
/// enviados ao peer (só conta upload real, não overhead de protocolo).
async fn apply_command<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    cmd: PeerCommand,
    ext: &ExtensionHandshake,
    pedidos_recebidos: &mut usize,
) -> Result<u64> {
    let mut enviados = 0;
    match cmd {
        PeerCommand::Request(reqs) => {
            for req in reqs {
                write_msg(writer, &Message::Request(req)).await?;
            }
        }
        PeerCommand::Cancel(req) => write_msg(writer, &Message::Cancel(req)).await?,
        PeerCommand::Have(piece) => write_msg(writer, &Message::Have(piece)).await?,
        PeerCommand::Choke(true) => write_msg(writer, &Message::Choke).await?,
        PeerCommand::Choke(false) => write_msg(writer, &Message::Unchoke).await?,
        PeerCommand::Interested(true) => write_msg(writer, &Message::Interested).await?,
        PeerCommand::Interested(false) => write_msg(writer, &Message::NotInterested).await?,
        PeerCommand::SendBlock(req, data) => {
            *pedidos_recebidos = pedidos_recebidos.saturating_sub(1);
            enviados = data.len() as u64;
            write_msg(
                writer,
                &Message::Piece {
                    index: req.piece,
                    begin: req.offset,
                    block: data,
                },
            )
            .await?;
        }
        PeerCommand::Reject(req) => {
            *pedidos_recebidos = pedidos_recebidos.saturating_sub(1);
            write_msg(writer, &Message::RejectRequest(req)).await?;
        }
        PeerCommand::RequestMetadata(piece) => {
            if let Some(id) = ext.id_of("ut_metadata") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: MetadataMessage::Request { piece }.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::SendMetadata(msg) => {
            if let Some(id) = ext.id_of("ut_metadata") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: msg.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::SendPex(pex) => {
            if let Some(id) = ext.id_of("ut_pex") {
                write_msg(
                    writer,
                    &Message::Extended {
                        id,
                        payload: pex.encode(),
                    },
                )
                .await?;
            }
        }
        PeerCommand::Shutdown => {}
    }
    Ok(enviados)
}

/// Traduz uma mensagem do peer em um evento para o engine.
fn handle_message(
    slot: Slot,
    msg: Message,
    ext: &mut ExtensionHandshake,
    pedidos_recebidos: &mut usize,
) -> Result<Option<PeerEvent>> {
    Ok(match msg {
        Message::KeepAlive => None,
        Message::Choke => Some(PeerEvent::Choked(slot, true)),
        Message::Unchoke => Some(PeerEvent::Choked(slot, false)),
        Message::Interested => Some(PeerEvent::Interested(slot, true)),
        Message::NotInterested => {
            // Perdeu o interesse: os pedidos pendentes dele morrem junto.
            *pedidos_recebidos = 0;
            Some(PeerEvent::Interested(slot, false))
        }
        Message::Have(piece) => Some(PeerEvent::Have(slot, piece)),
        Message::Bitfield(bits) => Some(PeerEvent::Bitfield(slot, bits)),
        Message::HaveAll => Some(PeerEvent::HaveAll(slot)),
        Message::HaveNone => Some(PeerEvent::HaveNone(slot)),
        Message::Request(req) => {
            if req.length > MAX_SERVE_LENGTH {
                bail!("peer pediu bloco de {} bytes", req.length);
            }
            *pedidos_recebidos += 1;
            if *pedidos_recebidos > MAX_INCOMING_REQUESTS {
                bail!("peer enfileirou {pedidos_recebidos} pedidos sem esperar");
            }
            Some(PeerEvent::BlockRequested(slot, req))
        }
        // O cancelamento só desfaz a contagem de pedidos pendentes; se o bloco
        // já estiver a caminho, mandá-lo mesmo assim é legítimo e o peer
        // descarta.
        Message::Cancel(_) => {
            *pedidos_recebidos = pedidos_recebidos.saturating_sub(1);
            None
        }
        Message::Piece {
            index,
            begin,
            block,
        } => Some(PeerEvent::Block {
            slot,
            request: BlockRequest {
                piece: index,
                offset: begin,
                length: block.len() as u32,
            },
            data: block,
        }),
        Message::RejectRequest(req) => Some(PeerEvent::Rejected(slot, req)),
        Message::Port(port) => Some(PeerEvent::DhtPort(slot, port)),
        // Sugestões da fast extension são dicas de cache do peer; o picker já
        // decide melhor que isso, então só registramos que ele tem a peça.
        Message::SuggestPiece(piece) | Message::AllowedFast(piece) => {
            Some(PeerEvent::Have(slot, piece))
        }
        Message::Extended { id: 0, payload } => {
            // O handshake de extensões pode ser reenviado a qualquer momento
            // para ligar ou desligar extensões; o último vale.
            *ext = ExtensionHandshake::decode(&payload)?;
            Some(PeerEvent::Extensions(slot, Box::new(ext.clone())))
        }
        Message::Extended { id, payload } => match id {
            extension::UT_METADATA_ID => {
                Some(PeerEvent::Metadata(slot, MetadataMessage::decode(&payload)?))
            }
            extension::UT_PEX_ID => Some(PeerEvent::Pex(PexMessage::decode(&payload)?)),
            // Extensão que anunciamos? Não. Ignorar é o comportamento correto.
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::BLOCK_SIZE;

    fn ext_vazio() -> ExtensionHandshake {
        ExtensionHandshake::default()
    }

    #[test]
    fn traduz_mensagens_em_eventos() {
        let mut ext = ext_vazio();
        let mut pend = 0;

        let ev = handle_message(3, Message::Unchoke, &mut ext, &mut pend).unwrap();
        assert!(matches!(ev, Some(PeerEvent::Choked(3, false))));

        let ev = handle_message(3, Message::Have(7), &mut ext, &mut pend).unwrap();
        assert!(matches!(ev, Some(PeerEvent::Have(3, 7))));

        let ev = handle_message(
            3,
            Message::Piece {
                index: 1,
                begin: BLOCK_SIZE,
                block: vec![0; 32],
            },
            &mut ext,
            &mut pend,
        )
        .unwrap();
        match ev {
            Some(PeerEvent::Block { request, data, .. }) => {
                assert_eq!(request.piece, 1);
                assert_eq!(request.offset, BLOCK_SIZE);
                assert_eq!(request.length, 32, "tamanho vem do bloco recebido");
                assert_eq!(data.len(), 32);
            }
            outro => panic!("esperava Block, veio {outro:?}"),
        }

        assert!(
            handle_message(3, Message::KeepAlive, &mut ext, &mut pend)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn corta_peer_que_abusa_de_pedidos() {
        let mut ext = ext_vazio();
        let mut pend = 0;
        let req = BlockRequest {
            piece: 0,
            offset: 0,
            length: BLOCK_SIZE,
        };

        for _ in 0..MAX_INCOMING_REQUESTS {
            handle_message(0, Message::Request(req), &mut ext, &mut pend).unwrap();
        }
        assert!(
            handle_message(0, Message::Request(req), &mut ext, &mut pend).is_err(),
            "fila cheia derruba o peer"
        );

        let gigante = BlockRequest {
            piece: 0,
            offset: 0,
            length: MAX_SERVE_LENGTH + 1,
        };
        let mut pend = 0;
        assert!(handle_message(0, Message::Request(gigante), &mut ext, &mut pend).is_err());
    }

    #[test]
    fn cancel_e_not_interested_esvaziam_a_fila() {
        let mut ext = ext_vazio();
        let mut pend = 0;
        let req = BlockRequest {
            piece: 0,
            offset: 0,
            length: 16,
        };

        handle_message(0, Message::Request(req), &mut ext, &mut pend).unwrap();
        handle_message(0, Message::Request(req), &mut ext, &mut pend).unwrap();
        assert_eq!(pend, 2);

        handle_message(0, Message::Cancel(req), &mut ext, &mut pend).unwrap();
        assert_eq!(pend, 1);
        handle_message(0, Message::NotInterested, &mut ext, &mut pend).unwrap();
        assert_eq!(pend, 0);
    }

    #[test]
    fn handshake_de_extensoes_atualiza_os_ids() {
        let mut ext = ext_vazio();
        let mut pend = 0;
        assert!(!ext.supports_metadata());

        let payload = ExtensionHandshake::ours(Some(1000), true, 6881).encode();
        let ev = handle_message(0, Message::Extended { id: 0, payload }, &mut ext, &mut pend)
            .unwrap();
        assert!(matches!(ev, Some(PeerEvent::Extensions(0, _))));
        assert!(ext.supports_metadata() && ext.supports_pex());
        assert_eq!(ext.metadata_size, Some(1000));

        // Agora que sabemos os ids, uma mensagem de metadata é reconhecida.
        let dados = MetadataMessage::Request { piece: 0 }.encode();
        let ev = handle_message(
            0,
            Message::Extended {
                id: extension::UT_METADATA_ID,
                payload: dados,
            },
            &mut ext,
            &mut pend,
        )
        .unwrap();
        assert!(matches!(ev, Some(PeerEvent::Metadata(0, _))));

        // Extensão desconhecida não derruba a conexão.
        let ev = handle_message(
            0,
            Message::Extended {
                id: 77,
                payload: vec![],
            },
            &mut ext,
            &mut pend,
        )
        .unwrap();
        assert!(ev.is_none());
    }
}
