//! torrra — um cliente BitTorrent de terminal.

mod bencode;
mod bitfield;
mod client_id;
mod compact;
mod dht;
mod engine;
mod id;
#[macro_use]
mod log;
mod magnet;
mod metainfo;
mod peer;
mod picker;
mod plataforma;
mod session;
mod stats;
mod storage;
mod tracker;
mod ui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use engine::Config;
use session::Session;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "torrra",
    version,
    about = "Cliente BitTorrent de terminal",
    long_about = "Baixa arquivos .torrent e magnet links, com DHT, PEX e interface de terminal."
)]
struct Args {
    /// Arquivos .torrent ou magnet links. Sem nenhum, a interface abre vazia
    /// e você adiciona pela tecla `a`. No Windows, arraste os arquivos para
    /// cima do executável.
    #[arg(value_name = "TORRENT|MAGNET")]
    fontes: Vec<String>,

    /// Diretório de destino (padrão: a pasta Downloads ao abrir pelo
    /// gerenciador de arquivos, o diretório atual pelo terminal)
    #[arg(short = 'o', long = "saida", value_name = "DIR")]
    saida: Option<PathBuf>,

    /// Porta de escuta (TCP e UDP)
    #[arg(short = 'p', long, default_value_t = 6881)]
    porta: u16,

    /// Máximo de peers conectados por torrent
    #[arg(long, default_value_t = 100, value_name = "N")]
    max_peers: usize,

    /// Quantos peers atendemos ao mesmo tempo
    #[arg(long, default_value_t = 6, value_name = "N")]
    slots_upload: usize,

    /// Baixa as peças em ordem, para assistir enquanto baixa
    #[arg(short = 's', long)]
    sequencial: bool,

    /// Desliga o DHT (só trackers)
    #[arg(long)]
    sem_dht: bool,

    /// Sai assim que o download terminar, em vez de continuar semeando
    #[arg(long)]
    sem_seed: bool,

    /// Limite de download, ex.: 2M, 500k
    #[arg(long, value_name = "TAXA")]
    limite_download: Option<String>,

    /// Limite de upload, ex.: 1M
    #[arg(long, value_name = "TAXA")]
    limite_upload: Option<String>,

    /// Sem interface: só imprime o progresso, uma linha por segundo
    #[arg(long)]
    simples: bool,

    /// Mostra o conteúdo de um .torrent e sai
    #[arg(long, value_name = "TORRENT")]
    info: Option<PathBuf>,
}

fn main() -> Result<()> {
    plataforma::preparar_console();

    // `parse` sairia na hora em caso de erro — e aberto pelo Explorer isso
    // fecha a janela antes de dar para ler a mensagem.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(e) => {
            let _ = e.print();
            plataforma::segurar_janela();
            std::process::exit(e.exit_code());
        }
    };

    let resultado = executar_tudo(args);
    if let Err(e) = &resultado {
        eprintln!("erro: {e:#}");
        plataforma::segurar_janela();
    }
    resultado
}

fn executar_tudo(args: Args) -> Result<()> {
    if let Some(caminho) = &args.info {
        return mostrar_info(caminho);
    }

    // O runtime é montado à mão para dar um nome útil às threads e dimensionar
    // o pool de I/O bloqueante, que é onde a verificação de peças roda.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("torrra")
        .max_blocking_threads(16)
        .build()?;
    runtime.block_on(executar(args))
}

/// Onde os arquivos vão parar, em ordem de precedência: a opção `-o`, um
/// `destino.txt` ao lado do executável, a pasta Downloads (quando o programa
/// foi aberto pelo gerenciador de arquivos) e, por fim, o diretório atual.
///
/// O diretório atual é péssimo padrão para quem abriu com um duplo clique: o
/// Explorer entrega o diretório do executável, e ninguém quer o download
/// dentro da pasta do programa.
fn destino(pedido: Option<PathBuf>) -> PathBuf {
    let downloads = plataforma::aberto_pelo_gerenciador()
        .then(plataforma::pasta_downloads)
        .flatten();
    escolher_destino(pedido, destino_fixado(), downloads)
}

fn escolher_destino(
    pedido: Option<PathBuf>,
    fixado: Option<PathBuf>,
    downloads: Option<PathBuf>,
) -> PathBuf {
    pedido
        .or(fixado)
        .or(downloads)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A mensagem que o usuário vê quando um arquivo não abre.
///
/// Arrastado do Explorer, o caminho vem inteiro — e ele sozinho enche a caixa
/// de avisos, empurrando para fora justamente o motivo. Fica o nome do arquivo
/// no lugar do caminho, e o nome vai na frente se o erro não o mencionar.
fn aviso_de_recusa(fonte: &str, e: &anyhow::Error) -> String {
    let nome = Path::new(fonte)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| fonte.to_string());
    let motivo = format!("{e:#}").replace(fonte, &nome);
    if motivo.contains(&nome) {
        motivo
    } else {
        format!("{nome}: {motivo}")
    }
}

/// `canonicalize` devolve o caminho com o prefixo `\\?\` no Windows. Ele é
/// legítimo, mas assusta quem só quer saber onde o arquivo foi parar.
fn exibir(caminho: &Path) -> String {
    let texto = caminho.display().to_string();
    texto.strip_prefix(r"\\?\").unwrap_or(&texto).to_string()
}

/// Um `destino.txt` ao lado do executável, com o caminho em uma linha, fixa a
/// pasta de destino para quem nunca vai passar por uma linha de comando.
fn destino_fixado() -> Option<PathBuf> {
    let ao_lado = std::env::current_exe().ok()?.parent()?.join("destino.txt");
    let conteudo = std::fs::read_to_string(ao_lado).ok()?;
    let linha = conteudo.lines().next()?.trim();
    (!linha.is_empty()).then(|| PathBuf::from(linha))
}

async fn executar(args: Args) -> Result<()> {
    let cfg = Config {
        dest: destino(args.saida.clone()),
        max_peers: args.max_peers.clamp(4, 1000),
        upload_slots: args.slots_upload.clamp(1, 50),
        listen_port: args.porta,
        sequential: args.sequencial,
        max_download: parse_taxa(args.limite_download.as_deref())?,
        max_upload: parse_taxa(args.limite_upload.as_deref())?,
        use_dht: !args.sem_dht,
        seed_after_complete: !args.sem_seed,
    };

    std::fs::create_dir_all(&cfg.dest)
        .with_context(|| format!("não consegui criar {}", cfg.dest.display()))?;
    // Só depois de existir dá para resolver `.` e os links no caminho.
    let onde = std::fs::canonicalize(&cfg.dest).unwrap_or_else(|_| cfg.dest.clone());

    let session = Session::start(cfg).await?;
    if args.simples {
        println!("destino: {}", exibir(&onde));
        println!("escutando na porta {}", session.port());
    }

    // Um arquivo inválido no meio de vários (típico ao arrastar um punhado de
    // coisas para cima do executável) não pode derrubar os que estão bons.
    let mut recusados = Vec::new();
    for fonte in &args.fontes {
        match session::parse_source(fonte) {
            Ok(source) => {
                session.add(source);
            }
            Err(e) => recusados.push(aviso_de_recusa(fonte, &e)),
        }
    }
    if args.simples {
        for aviso in &recusados {
            eprintln!("ignorado — {aviso}");
        }
        if session.count() == 0 {
            bail!("nenhum torrent válido para baixar");
        }
    }
    // Na interface os avisos não podem ir para o stderr: ela toma a tela toda,
    // e aberto pelo Explorer nem sobra terminal para lê-los depois.

    // Ctrl+C precisa avisar os trackers antes de sair, senão o torrent fica
    // marcado como ativo do lado deles por mais meia hora.
    let s = Arc::clone(&session);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ratatui::restore();
            s.shutdown().await;
            std::process::exit(0);
        }
    });

    let resultado = if args.simples {
        acompanhar_simples(&session).await
    } else {
        ui::run(Arc::clone(&session), recusados).await
    };

    session.shutdown().await;
    resultado
}

/// Modo sem interface: uma linha de progresso por segundo.
async fn acompanhar_simples(session: &Arc<Session>) -> Result<()> {
    use stats::{bytes, eta, rate};
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tick.tick().await;
        let snaps = session.snapshots();
        let mut tudo_pronto = !snaps.is_empty();

        for snap in &snaps {
            let alvo = snap.wanted.max(1);
            let fracao = (snap.downloaded as f64 / alvo as f64).clamp(0.0, 1.0);
            tudo_pronto &= snap.state_label == "semeando" || fracao >= 1.0;

            println!(
                "[{:>5.1}%] {} · {} de {} · ↓{} ↑{} · {} peers · faltam {}{}",
                fracao * 100.0,
                encurtar(&snap.name, 32),
                bytes(snap.downloaded),
                bytes(snap.wanted),
                rate(snap.download_rate),
                rate(snap.upload_rate),
                snap.connected,
                eta(snap.eta),
                snap.error
                    .as_ref()
                    .map(|e| format!(" · ERRO: {e}"))
                    .unwrap_or_default(),
            );
        }

        if tudo_pronto {
            println!("Concluído.");
            return Ok(());
        }
    }
}

fn mostrar_info(caminho: &PathBuf) -> Result<()> {
    use stats::bytes;
    let meta = metainfo::Metainfo::from_file(caminho)?;
    let info = &meta.info;

    println!("nome         {}", info.name);
    println!("info_hash    {}", info.info_hash);
    println!(
        "tamanho      {} ({} bytes)",
        bytes(info.total_length),
        info.total_length
    );
    println!(
        "peças        {} de {}",
        info.num_pieces(),
        bytes(info.piece_length as u64)
    );
    println!("privado      {}", if info.private { "sim" } else { "não" });
    if let Some(c) = &meta.comment {
        println!("comentário   {c}");
    }
    if let Some(c) = &meta.created_by {
        println!("criado por   {c}");
    }
    if let Some(t) = meta.creation_date {
        println!("criado em    {}", data_legivel(t));
    }
    if !meta.web_seeds.is_empty() {
        println!("web seeds    {}", meta.web_seeds.join(", "));
    }

    println!("\ntrackers");
    if meta.tiers.is_empty() {
        println!("  (nenhum — só DHT)");
    }
    for (i, tier) in meta.tiers.iter().enumerate() {
        for url in tier {
            println!("  [{i}] {url}");
        }
    }

    println!("\narquivos ({})", info.files.len());
    for arquivo in &info.files {
        println!("  {:>12}  {}", bytes(arquivo.length), arquivo.path.display());
    }
    Ok(())
}

/// Converte `2M`, `500k`, `1024` em bytes por segundo.
fn parse_taxa(texto: Option<&str>) -> Result<Option<u64>> {
    let Some(texto) = texto else {
        return Ok(None);
    };
    let texto = texto.trim();
    let (numero, fator) = match texto.chars().last() {
        Some('k' | 'K') => (&texto[..texto.len() - 1], 1024u64),
        Some('m' | 'M') => (&texto[..texto.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&texto[..texto.len() - 1], 1024 * 1024 * 1024),
        _ => (texto, 1),
    };
    let valor: f64 = numero
        .trim()
        .parse()
        .with_context(|| format!("taxa inválida: `{texto}`"))?;
    if valor <= 0.0 {
        bail!("a taxa precisa ser positiva: `{texto}`");
    }
    Ok(Some((valor * fator as f64) as u64))
}

/// Formata um timestamp Unix como `AAAA-MM-DD`, sem depender de uma
/// biblioteca de datas só para isto.
fn data_legivel(unix: i64) -> String {
    if unix <= 0 {
        return "—".to_string();
    }
    let dias_totais = unix / 86_400;
    let (mut ano, mut dias) = (1970, dias_totais);
    loop {
        let no_ano = if bissexto(ano) { 366 } else { 365 };
        if dias < no_ano {
            break;
        }
        dias -= no_ano;
        ano += 1;
    }
    let meses = [
        31,
        if bissexto(ano) { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut mes = 0;
    while mes < 12 && dias >= meses[mes] {
        dias -= meses[mes];
        mes += 1;
    }
    format!("{ano:04}-{:02}-{:02}", mes + 1, dias + 1)
}

fn bissexto(ano: i64) -> bool {
    (ano % 4 == 0 && ano % 100 != 0) || ano % 400 == 0
}

fn encurtar(texto: &str, limite: usize) -> String {
    if texto.chars().count() <= limite {
        return texto.to_string();
    }
    let cortado: String = texto.chars().take(limite.saturating_sub(1)).collect();
    format!("{cortado}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreta_limites_de_taxa() {
        assert_eq!(parse_taxa(None).unwrap(), None);
        assert_eq!(parse_taxa(Some("1024")).unwrap(), Some(1024));
        assert_eq!(parse_taxa(Some("500k")).unwrap(), Some(512_000));
        assert_eq!(parse_taxa(Some("2M")).unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(parse_taxa(Some("1.5M")).unwrap(), Some(1_572_864));
        assert_eq!(parse_taxa(Some(" 1G ")).unwrap(), Some(1 << 30));

        assert!(parse_taxa(Some("rápido")).is_err());
        assert!(parse_taxa(Some("0")).is_err());
        assert!(parse_taxa(Some("-5M")).is_err());
    }

    #[test]
    fn formata_datas() {
        assert_eq!(data_legivel(0), "—");
        assert_eq!(data_legivel(86_400), "1970-01-02");
        // 2024-02-29, para conferir o ano bissexto.
        assert_eq!(data_legivel(1_709_164_800), "2024-02-29");
        assert_eq!(data_legivel(1_735_689_600), "2025-01-01");
    }

    #[test]
    fn encurta_respeitando_caracteres() {
        assert_eq!(encurtar("curto", 10), "curto");
        assert_eq!(encurtar("abcdefghij", 5), "abcd…");
        // Acentos contam como um caractere, não como dois bytes.
        assert_eq!(encurtar("ãéîõü", 5), "ãéîõü");
    }

    #[test]
    fn cli_aceita_as_formas_de_uso() {
        let a = Args::try_parse_from(["torrra", "arquivo.torrent"]).unwrap();
        assert_eq!(a.fontes, ["arquivo.torrent"]);
        assert_eq!(a.porta, 6881);
        assert!(!a.sequencial);

        let a = Args::try_parse_from([
            "torrra", "-o", "/tmp", "-p", "51413", "-s", "--sem-dht", "m1", "m2",
        ])
        .unwrap();
        assert_eq!(a.saida, Some(PathBuf::from("/tmp")));
        assert_eq!(a.porta, 51413);
        assert!(a.sequencial && a.sem_dht);
        assert_eq!(a.fontes.len(), 2);

        // `--info` dispensa a fonte posicional.
        assert!(Args::try_parse_from(["torrra", "--info", "x.torrent"]).is_ok());
        // Sem argumento nenhum a interface abre vazia, esperando um magnet.
        let vazio = Args::try_parse_from(["torrra"]).unwrap();
        assert!(vazio.fontes.is_empty());
        assert_eq!(vazio.saida, None);
    }

    #[test]
    fn escolhe_o_destino_por_precedencia() {
        let opcao = || Some(PathBuf::from("/opcao"));
        let fixado = || Some(PathBuf::from("/fixado"));
        let downloads = || Some(PathBuf::from("/downloads"));

        // A opção da linha de comando ganha de tudo.
        assert_eq!(
            escolher_destino(opcao(), fixado(), downloads()),
            PathBuf::from("/opcao")
        );
        // Sem ela, o destino.txt ao lado do executável.
        assert_eq!(
            escolher_destino(None, fixado(), downloads()),
            PathBuf::from("/fixado")
        );
        // Sem os dois, a pasta Downloads — que só é oferecida quando o
        // programa foi aberto pelo gerenciador de arquivos.
        assert_eq!(
            escolher_destino(None, None, downloads()),
            PathBuf::from("/downloads")
        );
        // Pelo terminal, sem nada configurado, o diretório atual de sempre.
        assert_eq!(escolher_destino(None, None, None), PathBuf::from("."));
    }

    #[test]
    fn aviso_troca_o_caminho_pelo_nome_do_arquivo() {
        let caminho = r"C:\Users\alguem\Downloads\muito\fundo\quebrado.torrent";
        let erro = anyhow::anyhow!("bencode inválido")
            .context(format!("`{caminho}` não é um .torrent válido"));
        let aviso = aviso_de_recusa(caminho, &erro);

        assert!(!aviso.contains("Downloads"), "o caminho longo sai: {aviso}");
        assert!(aviso.contains("quebrado.torrent"), "o nome fica: {aviso}");
        assert!(aviso.contains("bencode inválido"), "e a causa: {aviso}");
    }

    #[test]
    fn aviso_nomeia_o_arquivo_quando_o_erro_nao_nomeia() {
        let erro = anyhow::anyhow!("disco cheio");
        let aviso = aviso_de_recusa(r"C:\tmp\filme.torrent", &erro);
        assert_eq!(aviso, "filme.torrent: disco cheio");
    }

    #[test]
    fn esconde_o_prefixo_de_caminho_longo() {
        assert_eq!(exibir(Path::new(r"\\?\C:\Users\x\Downloads")), r"C:\Users\x\Downloads");
        assert_eq!(exibir(Path::new("/home/x/Downloads")), "/home/x/Downloads");
    }
}
