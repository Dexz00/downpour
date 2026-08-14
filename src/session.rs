//! Sessão: o que é compartilhado entre todos os torrents — a porta de escuta,
//! o nó DHT e a lista de downloads ativos.

use crate::dht::{self, Dht};
use crate::engine::{self, Command, Config, Snapshot, Source, TorrentHandle};
use crate::id::Id20;
use crate::peer;
use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

pub struct Session {
    torrents: Mutex<Vec<TorrentHandle>>,
    dht: Option<Arc<Dht>>,
    cfg: Config,
    port: u16,
}

impl Session {
    /// Abre a porta de escuta e, se pedido, entra no DHT.
    ///
    /// Nenhuma das duas coisas é obrigatória: sem porta aberta o cliente ainda
    /// baixa (só não recebe conexões), e sem DHT ainda funciona com trackers.
    pub async fn start(mut cfg: Config) -> Result<Arc<Session>> {
        let listener = bind_listener(cfg.listen_port).await;
        let port = match &listener {
            Some(l) => l.local_addr().map(|a| a.port()).unwrap_or(cfg.listen_port),
            None => cfg.listen_port,
        };
        cfg.listen_port = port;

        let dht = if cfg.use_dht {
            match Dht::bind(port).await {
                Ok(d) => {
                    // Nós salvos da execução anterior costumam continuar de pé,
                    // e entram na tabela sem custo de rede. Os roteadores
                    // públicos ficam como plano B.
                    let salvos = carregar_nos_dht();
                    for (id, addr) in salvos {
                        d.add_node(crate::dht::krpc::NodeInfo { id, addr });
                    }

                    let d2 = Arc::clone(&d);
                    tokio::spawn(async move {
                        let bootstrap: Vec<String> =
                            dht::BOOTSTRAP.iter().map(|s| s.to_string()).collect();
                        d2.bootstrap(&bootstrap).await;
                        salvar_periodicamente(d2).await;
                    });
                    Some(d)
                }
                // Porta UDP ocupada não é motivo para abortar o download.
                Err(_) => None,
            }
        } else {
            None
        };

        let session = Arc::new(Session {
            torrents: Mutex::new(Vec::new()),
            dht,
            cfg,
            port,
        });

        if let Some(listener) = listener {
            let s = Arc::clone(&session);
            tokio::spawn(async move { accept_loop(s, listener).await });
        }
        Ok(session)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn add(&self, source: Source) -> Id20 {
        let info_hash = source.info_hash();
        let handle = engine::spawn(source, self.cfg.clone(), self.dht.clone());
        self.torrents.lock().unwrap().push(handle);
        info_hash
    }

    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.torrents
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.snapshot())
            .collect()
    }

    pub fn count(&self) -> usize {
        self.torrents.lock().unwrap().len()
    }

    /// Envia um comando ao torrent de índice `i`.
    pub fn command(&self, i: usize, cmd: Command) {
        if let Some(t) = self.torrents.lock().unwrap().get(i) {
            t.try_send(cmd);
        }
    }

    pub async fn shutdown(&self) {
        {
            let torrents = self.torrents.lock().unwrap();
            for t in torrents.iter() {
                t.try_send(Command::Shutdown);
            }
        }
        if let Some(dht) = &self.dht {
            salvar_nos_dht(&dht.snapshot());
        }
        // Dá um instante para o `stopped` chegar aos trackers antes de sair.
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
}

/// Arquivo onde guardamos os nós do DHT entre execuções.
fn caminho_cache() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("torrra"))
}

fn carregar_nos_dht() -> Vec<(Id20, SocketAddr)> {
    let Some(arquivo) = caminho_cache().map(|d| d.join("dht-nodes.txt")) else {
        return Vec::new();
    };
    let Ok(texto) = std::fs::read_to_string(arquivo) else {
        return Vec::new();
    };
    texto
        .lines()
        .filter_map(|linha| {
            let (id, addr) = linha.split_once(' ')?;
            Some((Id20::from_hex(id).ok()?, addr.trim().parse().ok()?))
        })
        .take(400)
        .collect()
}

fn salvar_nos_dht(nos: &[(Id20, SocketAddr)]) {
    if nos.is_empty() {
        return;
    }
    let Some(dir) = caminho_cache() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let conteudo: String = nos
        .iter()
        .take(400)
        .map(|(id, addr)| format!("{} {addr}\n", id.to_hex()))
        .collect();
    // Falhar aqui não afeta o download; o DHT só volta a partir do zero.
    let _ = std::fs::write(dir.join("dht-nodes.txt"), conteudo);
}

async fn salvar_periodicamente(dht: Arc<Dht>) {
    let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
    tick.tick().await; // a primeira dispara na hora
    loop {
        tick.tick().await;
        salvar_nos_dht(&dht.snapshot());
    }
}

/// Tenta a porta pedida e, se ocupada, deixa o sistema escolher uma.
async fn bind_listener(porta: u16) -> Option<TcpListener> {
    match TcpListener::bind(("0.0.0.0", porta)).await {
        Ok(l) => Some(l),
        Err(_) => TcpListener::bind(("0.0.0.0", 0)).await.ok(),
    }
}

/// Aceita conexões e as encaminha ao torrent dono do info_hash do handshake.
async fn accept_loop(session: Arc<Session>, listener: TcpListener) {
    loop {
        let Ok((mut socket, addr)) = listener.accept().await else {
            continue;
        };
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            // Ler o handshake aqui é o que permite saber a qual torrent a
            // conexão pertence antes de criar qualquer estado para ela.
            let Ok(handshake) = peer::read_incoming_handshake(&mut socket).await else {
                return;
            };
            let alvo = {
                let torrents = session.torrents.lock().unwrap();
                torrents
                    .iter()
                    .position(|t| t.info_hash == handshake.info_hash)
            };
            let Some(i) = alvo else {
                return; // torrent desconhecido: descarta
            };
            session.command(i, Command::Incoming(Box::new((socket, addr, handshake))));
        });
    }
}

/// Lê um `.torrent` do disco ou um magnet a partir do que o usuário digitou.
pub fn parse_source(entrada: &str) -> Result<Source> {
    if entrada.starts_with("magnet:") {
        return Ok(Source::Magnet(Box::new(crate::magnet::Magnet::parse(
            entrada,
        )?)));
    }
    // Sem contexto extra aqui: `from_file` já nomeia o arquivo, e repetir o
    // caminho em cada nível deixa a mensagem ilegível quando ela é longa.
    Ok(Source::Torrent(Box::new(
        crate::metainfo::Metainfo::from_file(entrada)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prova de rede do ciclo pausar/retomar, contra um enxame real.
    ///
    /// `TORRENT=x.torrent cargo test ciclo_pausa_real -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn ciclo_pausa_real() {
        let caminho = std::env::var("TORRENT").expect("defina TORRENT=caminho.torrent");
        let destino = std::env::temp_dir().join("torrra-pausa");
        let _ = std::fs::remove_dir_all(&destino);

        let s = Session::start(Config {
            dest: destino.clone(),
            listen_port: 0,
            ..Default::default()
        })
        .await
        .unwrap();
        s.add(parse_source(&caminho).unwrap());

        let esperar_baixando = |rotulo: &'static str| {
            let s = Arc::clone(&s);
            async move {
                for _ in 0..60 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let snap = &s.snapshots()[0];
                    if snap.download_rate > 100.0 * 1024.0 {
                        println!(
                            "{rotulo}: {:.1} MiB/s com {} peers",
                            snap.download_rate / 1024.0 / 1024.0,
                            snap.connected
                        );
                        return true;
                    }
                }
                false
            }
        };

        assert!(esperar_baixando("antes da pausa").await, "não engatou");

        s.command(0, Command::Pause);
        tokio::time::sleep(Duration::from_secs(5)).await;
        let pausado = &s.snapshots()[0];
        println!("pausado: {} peers", pausado.connected);
        assert_eq!(pausado.state_label, "pausado");
        assert_eq!(pausado.connected, 0, "pausar solta os peers");

        s.command(0, Command::Resume);
        assert!(
            esperar_baixando("depois de retomar").await,
            "não voltou a baixar depois de retomar"
        );

        let _ = std::fs::remove_dir_all(&destino);
    }

    #[test]
    fn identifica_magnet_e_arquivo() {
        let m = parse_source("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567")
            .unwrap();
        assert!(matches!(m, Source::Magnet(_)));
        assert!(parse_source("nao-existe.torrent").is_err());
    }

    #[tokio::test]
    async fn sessao_abre_porta_e_aceita_torrents() {
        let cfg = Config {
            listen_port: 0,
            use_dht: false,
            ..Default::default()
        };
        let s = Session::start(cfg).await.unwrap();
        assert!(s.port() > 0, "o SO escolheu uma porta livre");
        assert_eq!(s.count(), 0);

        let magnet = crate::magnet::Magnet::parse(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=teste",
        )
        .unwrap();
        let ih = s.add(Source::Magnet(Box::new(magnet)));
        assert_eq!(s.count(), 1);
        assert_eq!(ih.to_hex(), "0123456789abcdef0123456789abcdef01234567");

        let snaps = s.snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].name, "teste");
    }
}
