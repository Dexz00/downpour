//! Interface de terminal.
//!
//! A tela é montada a partir de um `Snapshot` — a UI nunca toca no estado do
//! engine, só lê a fotografia mais recente. Isso mantém o desenho fora do
//! caminho crítico do download.

use crate::engine::{Command, Snapshot};
use crate::picker::{PickMode, Priority};
use crate::session::Session;
use crate::stats::{bytes, eta, rate};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Gauge, List, ListItem, Padding, Paragraph, Row, Sparkline,
    Table, Tabs, Wrap,
};
use std::sync::Arc;
use std::time::Duration;

/// Intervalo de redesenho. 10 fps é suave e não briga com o download por CPU.
const FRAME: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Aba {
    Geral,
    Peers,
    Arquivos,
    Trackers,
}

impl Aba {
    const TODAS: [Aba; 4] = [Aba::Geral, Aba::Peers, Aba::Arquivos, Aba::Trackers];

    fn titulo(self) -> &'static str {
        match self {
            Aba::Geral => "Geral",
            Aba::Peers => "Peers",
            Aba::Arquivos => "Arquivos",
            Aba::Trackers => "Trackers",
        }
    }
}

/// Caixa de texto para colar um magnet ou caminho sem sair da interface.
struct Entrada {
    texto: String,
    erro: Option<String>,
}

struct App {
    aba: Aba,
    torrent: usize,
    /// Índice selecionado na lista da aba atual.
    selecao: usize,
    /// `Some` enquanto o usuário está digitando um novo torrent.
    entrada: Option<Entrada>,
    /// Arquivos recusados na abertura. Ficam na tela até a primeira tecla.
    avisos: Vec<String>,
    sair: bool,
}

impl Default for App {
    fn default() -> Self {
        App {
            aba: Aba::Geral,
            torrent: 0,
            selecao: 0,
            entrada: None,
            avisos: Vec::new(),
            sair: false,
        }
    }
}

// -- paleta ----------------------------------------------------------------

const ACENTO: Color = Color::Rgb(126, 201, 255);
const OK: Color = Color::Rgb(126, 220, 140);
const ATENCAO: Color = Color::Rgb(240, 190, 100);
const RUIM: Color = Color::Rgb(240, 120, 120);
const APAGADO: Color = Color::Rgb(120, 128, 140);

pub async fn run(session: Arc<Session>, avisos: Vec<String>) -> Result<()> {
    let mut terminal = ratatui::init();
    // Colar um magnet manda dezenas de caracteres de uma vez; com bracketed
    // paste eles chegam como um evento só, em vez de um por tecla.
    let _ = crossterm::execute!(std::io::stdout(), event::EnableBracketedPaste);

    let mut app = App {
        avisos,
        ..App::default()
    };

    let resultado = loop {
        let snaps = session.snapshots();
        if app.torrent >= snaps.len() {
            app.torrent = snaps.len().saturating_sub(1);
        }

        if let Err(e) = terminal.draw(|frame| desenhar(frame, &app, &snaps)) {
            break Err(e.into());
        }
        if let Err(e) = tratar_eventos(&mut app, &session, &snaps).await {
            break Err(e);
        }
        if app.sair {
            break Ok(());
        }
    };

    let _ = crossterm::execute!(std::io::stdout(), event::DisableBracketedPaste);
    ratatui::restore();
    resultado
}

async fn tratar_eventos(app: &mut App, session: &Arc<Session>, snaps: &[Snapshot]) -> Result<()> {
    if !event::poll(FRAME)? {
        return Ok(());
    }
    // Drena tudo que estiver pendente antes de redesenhar: colar texto sem
    // bracketed paste vira uma rajada de teclas, e uma por quadro ficaria lento.
    loop {
        match event::read()? {
            Event::Paste(texto) => {
                app.avisos.clear();
                if let Some(entrada) = &mut app.entrada {
                    entrada.texto.push_str(texto.trim());
                    entrada.erro = None;
                } else {
                    // Colar sem a caixa aberta abre a caixa com o conteúdo.
                    app.entrada = Some(Entrada {
                        texto: texto.trim().to_string(),
                        erro: None,
                    });
                }
            }
            Event::Key(tecla) if tecla.kind == KeyEventKind::Press => {
                // A primeira tecla dispensa os avisos e não faz mais nada:
                // ninguém deve fechar um erro sem ter tido chance de lê-lo.
                if !app.avisos.is_empty() {
                    app.avisos.clear();
                } else if app.entrada.is_some() {
                    tecla_na_entrada(app, session, tecla);
                } else {
                    tecla_normal(app, session, snaps, tecla).await;
                }
            }
            _ => {}
        }
        if app.sair || !event::poll(Duration::ZERO)? {
            return Ok(());
        }
    }
}

/// Teclas enquanto a caixa de adicionar torrent está aberta.
fn tecla_na_entrada(app: &mut App, session: &Arc<Session>, tecla: event::KeyEvent) {
    let Some(entrada) = &mut app.entrada else {
        return;
    };
    match tecla.code {
        KeyCode::Esc => app.entrada = None,
        KeyCode::Backspace => {
            entrada.texto.pop();
            entrada.erro = None;
        }
        KeyCode::Char('u') if tecla.modifiers.contains(KeyModifiers::CONTROL) => {
            entrada.texto.clear();
            entrada.erro = None;
        }
        KeyCode::Char(c) => {
            entrada.texto.push(c);
            entrada.erro = None;
        }
        KeyCode::Enter => {
            let alvo = entrada.texto.trim().trim_matches('"').to_string();
            if alvo.is_empty() {
                app.entrada = None;
                return;
            }
            match crate::session::parse_source(&alvo) {
                Ok(source) => {
                    session.add(source);
                    app.entrada = None;
                    app.torrent = session.count().saturating_sub(1);
                }
                Err(e) => entrada.erro = Some(format!("{e:#}")),
            }
        }
        _ => {}
    }
}

async fn tecla_normal(
    app: &mut App,
    session: &Arc<Session>,
    snaps: &[Snapshot],
    tecla: event::KeyEvent,
) {
    let atual = app.torrent;
    match tecla.code {
        KeyCode::Char('q') | KeyCode::Esc => app.sair = true,
        KeyCode::Char('c') if tecla.modifiers.contains(KeyModifiers::CONTROL) => app.sair = true,

        KeyCode::Tab | KeyCode::Right => {
            let i = Aba::TODAS.iter().position(|a| *a == app.aba).unwrap_or(0);
            app.aba = Aba::TODAS[(i + 1) % Aba::TODAS.len()];
            app.selecao = 0;
        }
        KeyCode::BackTab | KeyCode::Left => {
            let i = Aba::TODAS.iter().position(|a| *a == app.aba).unwrap_or(0);
            app.aba = Aba::TODAS[(i + Aba::TODAS.len() - 1) % Aba::TODAS.len()];
            app.selecao = 0;
        }
        KeyCode::Char(c @ '1'..='4') => {
            app.aba = Aba::TODAS[c as usize - '1' as usize];
            app.selecao = 0;
        }

        KeyCode::Down | KeyCode::Char('j') => app.selecao += 1,
        KeyCode::Up | KeyCode::Char('k') => app.selecao = app.selecao.saturating_sub(1),
        KeyCode::Char('n') => {
            if !snaps.is_empty() {
                app.torrent = (app.torrent + 1) % snaps.len();
            }
        }

        KeyCode::Char('p') => {
            let pausado = snaps
                .get(atual)
                .is_some_and(|s| s.state_label == "pausado");
            session.command(
                atual,
                if pausado { Command::Resume } else { Command::Pause },
            );
        }
        KeyCode::Char('s') => {
            let sequencial = snaps.get(atual).is_some_and(|s| s.sequential);
            session.command(
                atual,
                Command::SetMode(if sequencial {
                    PickMode::Rarest
                } else {
                    PickMode::Sequential
                }),
            );
        }
        KeyCode::Char('r') => session.command(atual, Command::Recheck),

        // Prioridade do arquivo selecionado, na aba de arquivos.
        KeyCode::Char(' ') if app.aba == Aba::Arquivos => {
            if let Some(snap) = snaps.get(atual)
                && let Some(arquivo) = snap.files.get(app.selecao)
            {
                let nova = match arquivo.priority {
                    Priority::Skip => Priority::Normal,
                    _ => Priority::Skip,
                };
                session.command(atual, Command::SetFilePriority(app.selecao, nova));
            }
        }
        KeyCode::Char('+') | KeyCode::Char('=') if app.aba == Aba::Arquivos => {
            if let Some(snap) = snaps.get(atual)
                && let Some(arquivo) = snap.files.get(app.selecao)
            {
                let nova = match arquivo.priority {
                    Priority::Skip => Priority::Low,
                    Priority::Low => Priority::Normal,
                    _ => Priority::High,
                };
                session.command(atual, Command::SetFilePriority(app.selecao, nova));
            }
        }
        KeyCode::Char('-') if app.aba == Aba::Arquivos => {
            if let Some(snap) = snaps.get(atual)
                && let Some(arquivo) = snap.files.get(app.selecao)
            {
                let nova = match arquivo.priority {
                    Priority::High => Priority::Normal,
                    Priority::Normal => Priority::Low,
                    _ => Priority::Skip,
                };
                session.command(atual, Command::SetFilePriority(app.selecao, nova));
            }
        }

        // Adicionar um torrent sem sair da interface.
        KeyCode::Char('a') | KeyCode::Insert => {
            app.entrada = Some(Entrada {
                texto: String::new(),
                erro: None,
            });
        }
        _ => {}
    }
}

// -- desenho ---------------------------------------------------------------

fn desenhar(frame: &mut Frame, app: &App, snaps: &[Snapshot]) {
    let area = frame.area();
    let Some(snap) = snaps.get(app.torrent) else {
        desenhar_boas_vindas(frame, area);
        if let Some(entrada) = &app.entrada {
            desenhar_entrada(frame, area, entrada);
        }
        desenhar_avisos(frame, area, &app.avisos);
        return;
    };

    let [cabecalho, corpo, rodape] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(area);

    desenhar_cabecalho(frame, cabecalho, snap, snaps.len(), app.torrent);

    let [abas, conteudo] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(corpo);
    desenhar_abas(frame, abas, app.aba);

    match app.aba {
        Aba::Geral => desenhar_geral(frame, conteudo, snap),
        Aba::Peers => desenhar_peers(frame, conteudo, snap),
        Aba::Arquivos => desenhar_arquivos(frame, conteudo, snap, app.selecao),
        Aba::Trackers => desenhar_trackers(frame, conteudo, snap),
    }

    desenhar_rodape(frame, rodape, app.aba);

    if let Some(entrada) = &app.entrada {
        desenhar_entrada(frame, area, entrada);
    }
    desenhar_avisos(frame, area, &app.avisos);
}

/// Caixa com os arquivos que não puderam ser abertos. Sem terminal por trás,
/// esta é a única chance de o usuário saber por que faltou um download.
fn desenhar_avisos(frame: &mut Frame, area: Rect, avisos: &[String]) {
    if avisos.is_empty() {
        return;
    }
    let largura = area.width.saturating_sub(8).clamp(24, 76);
    let util = largura.saturating_sub(4) as usize; // bordas e respiro lateral

    // O texto é quebrado aqui, e não pelo `Wrap`, porque a altura da caixa
    // depende de quantas linhas cada aviso vira. Sobram 5 para as bordas, o
    // respiro e o rodapé.
    let teto = (area.height.saturating_sub(5) as usize).max(1);
    let mut texto: Vec<String> = Vec::new();
    let mut mostrados = 0;
    for aviso in avisos {
        let pedacos = quebrar(aviso, util);
        if !texto.is_empty() && texto.len() + pedacos.len() > teto {
            break;
        }
        texto.extend(pedacos);
        mostrados += 1;
        if texto.len() >= teto {
            break;
        }
    }
    texto.truncate(teto);

    let mut linhas: Vec<Line> = texto
        .into_iter()
        .map(|linha| Line::from(Span::styled(linha, Style::new().fg(RUIM))))
        .collect();
    if mostrados < avisos.len() {
        linhas.push(Line::from(Span::styled(
            format!("… e mais {}", avisos.len() - mostrados),
            Style::new().fg(APAGADO),
        )));
    }
    linhas.push(Line::default());
    linhas.push(Line::from(Span::styled(
        "qualquer tecla continua",
        Style::new().fg(APAGADO),
    )));

    let altura = (linhas.len() + 2).min(area.height as usize) as u16;
    let caixa = area_central(area, largura, altura);
    frame.render_widget(ratatui::widgets::Clear, caixa);

    let bloco = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ATENCAO))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            if avisos.len() == 1 {
                " não consegui abrir 1 arquivo ".to_string()
            } else {
                format!(" não consegui abrir {} arquivos ", avisos.len())
            },
            Style::new().fg(ATENCAO).add_modifier(Modifier::BOLD),
        ));
    let interno = bloco.inner(caixa);
    frame.render_widget(bloco, caixa);
    frame.render_widget(Paragraph::new(linhas), interno);
}

/// Quebra em palavras. Uma palavra sozinha maior que a largura é cortada — é o
/// caso de um magnet colado inteiro, e não há o que preservar nele.
fn quebrar(texto: &str, largura: usize) -> Vec<String> {
    if largura == 0 {
        return Vec::new();
    }
    let mut linhas = Vec::new();
    let mut atual = String::new();
    for palavra in texto.split_whitespace() {
        if atual.is_empty() {
            atual = palavra.chars().take(largura).collect();
        } else if atual.chars().count() + 1 + palavra.chars().count() <= largura {
            atual.push(' ');
            atual.push_str(palavra);
        } else {
            linhas.push(std::mem::take(&mut atual));
            atual = palavra.chars().take(largura).collect();
        }
    }
    if !atual.is_empty() {
        linhas.push(atual);
    }
    linhas
}

/// Logo em blocos, desenhado à mão para caber em terminais estreitos.
const LOGO: [&str; 5] = [
    "▀█▀ █▀█ █▀█ █▀█ █▀█ ▄▀█",
    " █  █ █ █▀▄ █▀▄ █▀▄ █▀█",
    " ▀  ▀▀▀ ▀ ▀ ▀ ▀ ▀ ▀ ▀ ▀",
    "",
    "cliente bittorrent",
];

/// Tela mostrada enquanto não há nenhum torrent na sessão.
fn desenhar_boas_vindas(frame: &mut Frame, area: Rect) {
    let bloco = moldura(" torrra ");
    let interno = bloco.inner(area);
    frame.render_widget(bloco, area);
    if interno.height < 6 {
        frame.render_widget(
            Paragraph::new("arraste um .torrent ou aperte [a] para colar um magnet")
                .style(Style::new().fg(APAGADO)),
            interno,
        );
        return;
    }

    let mut linhas: Vec<Line> = Vec::new();
    linhas.push(Line::default());
    for (i, arte) in LOGO.iter().enumerate() {
        let estilo = if i < 3 {
            Style::new().fg(ACENTO).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(APAGADO)
        };
        linhas.push(Line::from(Span::styled(*arte, estilo)).centered());
    }
    linhas.push(Line::default());
    linhas.push(Line::default());

    let atalho = |tecla: &str, texto: &str| {
        Line::from(vec![
            Span::styled(
                format!(" {tecla} "),
                Style::new().fg(Color::Black).bg(ACENTO),
            ),
            Span::styled(format!("  {texto}"), Style::new().fg(Color::Reset)),
        ])
        .centered()
    };

    linhas.push(
        Line::from(Span::styled(
            "cole um magnet link — a caixa abre sozinha",
            Style::new().fg(OK),
        ))
        .centered(),
    );
    linhas.push(Line::default());
    linhas.push(atalho("a", "digitar um magnet ou caminho"));
    linhas.push(Line::default());
    linhas.push(atalho("q", "sair"));
    linhas.push(Line::default());
    linhas.push(
        Line::from(Span::styled(
            "arquivos .torrent: arraste para cima do torrra.exe",
            Style::new().fg(APAGADO),
        ))
        .centered(),
    );

    // Centraliza verticalmente o que couber.
    let sobra = interno.height.saturating_sub(linhas.len() as u16) / 2;
    let alvo = Rect {
        y: interno.y + sobra,
        height: interno.height - sobra,
        ..interno
    };
    frame.render_widget(Paragraph::new(linhas), alvo);
}

/// Caixa flutuante para digitar ou colar um magnet/caminho.
fn desenhar_entrada(frame: &mut Frame, area: Rect, entrada: &Entrada) {
    let largura = area.width.saturating_sub(8).clamp(20, 76);
    let altura = if entrada.erro.is_some() { 7 } else { 5 };
    let caixa = area_central(area, largura, altura);

    frame.render_widget(ratatui::widgets::Clear, caixa);
    let bloco = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACENTO))
        .title(Span::styled(
            " adicionar torrent ",
            Style::new().fg(ACENTO).add_modifier(Modifier::BOLD),
        ));
    let interno = bloco.inner(caixa);
    frame.render_widget(bloco, caixa);

    // Mostra o fim do texto quando ele passa da largura da caixa.
    let visivel = interno.width.saturating_sub(2) as usize;
    let texto: String = if entrada.texto.chars().count() > visivel {
        entrada
            .texto
            .chars()
            .skip(entrada.texto.chars().count() - visivel)
            .collect()
    } else {
        entrada.texto.clone()
    };

    let mut linhas = vec![
        Line::from(Span::styled(
            "cole o magnet ou o caminho do .torrent",
            Style::new().fg(APAGADO),
        )),
        Line::from(vec![
            Span::styled("› ", Style::new().fg(ACENTO)),
            Span::raw(texto),
            Span::styled("█", Style::new().fg(ACENTO)),
        ]),
    ];
    if let Some(erro) = &entrada.erro {
        linhas.push(Line::default());
        linhas.push(Line::from(Span::styled(
            erro.chars().take(visivel).collect::<String>(),
            Style::new().fg(RUIM),
        )));
    }
    linhas.push(Line::from(Span::styled(
        "enter confirma · esc cancela",
        Style::new().fg(APAGADO),
    )));

    frame.render_widget(Paragraph::new(linhas), interno);
}

/// Retângulo centralizado dentro de `area`.
fn area_central(area: Rect, largura: u16, altura: u16) -> Rect {
    let largura = largura.min(area.width);
    let altura = altura.min(area.height);
    Rect {
        x: area.x + (area.width - largura) / 2,
        y: area.y + (area.height - altura) / 2,
        width: largura,
        height: altura,
    }
}

/// Toma o título por valor para o `Block` não ficar preso a um `format!`
/// temporário na chamada.
fn moldura(titulo: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(APAGADO))
        .title(Span::styled(
            titulo.into(),
            Style::new().fg(ACENTO).add_modifier(Modifier::BOLD),
        ))
}

fn desenhar_cabecalho(
    frame: &mut Frame,
    area: Rect,
    snap: &Snapshot,
    total_torrents: usize,
    indice: usize,
) {
    let titulo = if total_torrents > 1 {
        format!(" {} ({}/{}) ", snap.name, indice + 1, total_torrents)
    } else {
        format!(" {} ", snap.name)
    };
    let bloco = moldura(titulo);
    let interno = bloco.inner(area);
    frame.render_widget(bloco, area);

    let [linha_estado, linha_barra, linha_num] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(interno);

    // Linha 1: estado e sinalizadores.
    let cor_estado = match snap.state_label.as_str() {
        "semeando" => OK,
        "baixando" => ACENTO,
        "erro" => RUIM,
        "pausado" => APAGADO,
        _ => ATENCAO,
    };
    let mut partes = vec![
        Span::styled(
            format!("● {}", snap.state_label),
            Style::new().fg(cor_estado).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
    ];
    if let Some(erro) = &snap.error {
        partes.push(Span::styled(erro.clone(), Style::new().fg(RUIM)));
    } else {
        partes.push(Span::styled(
            format!("{} peers", snap.connected),
            Style::new().fg(APAGADO),
        ));
        partes.push(Span::styled(
            format!("  ({} na fila)", snap.candidates),
            Style::new().fg(APAGADO),
        ));
        if snap.dht_nodes > 0 {
            partes.push(Span::styled(
                format!("   DHT {} nós", snap.dht_nodes),
                Style::new().fg(APAGADO),
            ));
        }
        if snap.sequential {
            partes.push(Span::styled("   [sequencial]", Style::new().fg(ATENCAO)));
        }
        if snap.endgame {
            partes.push(Span::styled("   [endgame]", Style::new().fg(ATENCAO)));
        }
    }
    frame.render_widget(Line::from(partes), linha_estado);

    // Linha 2: barra de progresso.
    let alvo = snap.wanted.max(1);
    let fracao = (snap.downloaded as f64 / alvo as f64).clamp(0.0, 1.0);
    let rotulo = if snap.state_label == "verificando" {
        format!("verificando… {:.0}%", snap.check_progress * 100.0)
    } else {
        format!(
            "{:.1}%  {} / {}",
            fracao * 100.0,
            bytes(snap.downloaded),
            bytes(snap.wanted)
        )
    };
    let mostrado = if snap.state_label == "verificando" {
        snap.check_progress
    } else {
        fracao
    };
    frame.render_widget(
        Gauge::default()
            .ratio(mostrado.clamp(0.0, 1.0))
            .label(rotulo)
            .gauge_style(Style::new().fg(if fracao >= 1.0 { OK } else { ACENTO }))
            .use_unicode(true),
        linha_barra,
    );

    // Linha 3: números.
    let numeros = Line::from(vec![
        Span::styled("↓ ", Style::new().fg(OK)),
        Span::styled(
            rate(snap.download_rate),
            Style::new().fg(OK).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled("↑ ", Style::new().fg(ACENTO)),
        Span::styled(rate(snap.upload_rate), Style::new().fg(ACENTO)),
        Span::raw("    "),
        Span::styled("faltam ", Style::new().fg(APAGADO)),
        Span::raw(eta(snap.eta)),
        Span::raw("    "),
        Span::styled("enviado ", Style::new().fg(APAGADO)),
        Span::raw(bytes(snap.uploaded)),
        Span::raw("    "),
        Span::styled("peças ", Style::new().fg(APAGADO)),
        Span::raw(format!("{}/{}", snap.have_pieces, snap.total_pieces)),
    ]);
    frame.render_widget(Paragraph::new(numeros), linha_num);
}

fn desenhar_abas(frame: &mut Frame, area: Rect, atual: Aba) {
    let indice = Aba::TODAS.iter().position(|a| *a == atual).unwrap_or(0);
    let titulos: Vec<Line> = Aba::TODAS
        .iter()
        .enumerate()
        .map(|(i, a)| {
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), Style::new().fg(APAGADO)),
                Span::raw(a.titulo()),
            ])
        })
        .collect();
    frame.render_widget(
        Tabs::new(titulos)
            .select(indice)
            .highlight_style(Style::new().fg(ACENTO).add_modifier(Modifier::BOLD))
            .divider(Span::styled("│", Style::new().fg(APAGADO))),
        area,
    );
}

fn desenhar_geral(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let [grafico, mapa] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // Gráfico de velocidade.
    let [g_down, g_up] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas(grafico);
    sparkline(
        frame,
        g_down,
        &snap.down_history,
        " download ",
        OK,
        snap.download_rate,
    );
    sparkline(
        frame,
        g_up,
        &snap.up_history,
        " upload ",
        ACENTO,
        snap.upload_rate,
    );

    // Mapa de peças.
    let bloco = moldura(format!(
        " peças  ({} de {}) ",
        snap.have_pieces, snap.total_pieces
    ));
    let interno = bloco.inner(mapa);
    frame.render_widget(bloco, mapa);
    frame.render_widget(mapa_de_pecas(snap, interno), interno);
}

fn sparkline(frame: &mut Frame, area: Rect, historico: &[f64], titulo: &str, cor: Color, atual: f64) {
    let bloco = moldura(format!("{titulo}{} ", rate(atual)));
    let interno = bloco.inner(area);
    frame.render_widget(bloco, area);
    if interno.height == 0 {
        return;
    }

    // A escala acompanha o pico recente, senão um download rápido achata o
    // gráfico assim que a velocidade cai.
    let pico = historico.iter().copied().fold(1.0f64, f64::max);
    let dados: Vec<u64> = historico
        .iter()
        .map(|v| ((v / pico) * 100.0) as u64)
        .collect();
    frame.render_widget(
        Sparkline::default()
            .data(&dados)
            .max(100)
            .style(Style::new().fg(cor)),
        interno,
    );
}

/// Desenha as peças como um mapa de blocos: cheio, em progresso ou vazio.
fn mapa_de_pecas(snap: &Snapshot, area: Rect) -> Paragraph<'static> {
    if snap.total_pieces == 0 || area.width == 0 || area.height == 0 {
        return Paragraph::new(Span::styled(
            "aguardando metadata…",
            Style::new().fg(APAGADO),
        ));
    }

    let celulas = (area.width as usize) * (area.height as usize);
    // Cada célula da tela representa um trecho contíguo de peças.
    let por_celula = snap.total_pieces.div_ceil(celulas).max(1);
    let em_progresso: std::collections::HashMap<u32, f32> =
        snap.partials.iter().copied().collect();

    let mut linhas = Vec::with_capacity(area.height as usize);
    let mut spans = Vec::with_capacity(area.width as usize);

    for celula in 0..celulas {
        let inicio = celula * por_celula;
        if inicio >= snap.total_pieces {
            break;
        }
        let fim = (inicio + por_celula).min(snap.total_pieces);

        let mut completas = 0;
        let mut parciais = 0;
        for piece in inicio..fim {
            if bit_ligado(&snap.pieces, piece) {
                completas += 1;
            } else if em_progresso.contains_key(&(piece as u32)) {
                parciais += 1;
            }
        }

        let total = fim - inicio;
        let (glifo, cor) = if completas == total {
            ('█', OK)
        } else if completas * 2 >= total {
            ('▓', OK)
        } else if completas > 0 {
            ('▒', ACENTO)
        } else if parciais > 0 {
            ('░', ATENCAO)
        } else {
            ('·', APAGADO)
        };
        spans.push(Span::styled(glifo.to_string(), Style::new().fg(cor)));

        if spans.len() == area.width as usize {
            linhas.push(Line::from(std::mem::take(&mut spans)));
        }
    }
    if !spans.is_empty() {
        linhas.push(Line::from(spans));
    }
    Paragraph::new(linhas)
}

fn bit_ligado(bits: &[u8], indice: usize) -> bool {
    bits.get(indice / 8)
        .is_some_and(|b| b & (0x80 >> (indice % 8)) != 0)
}

fn desenhar_peers(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let cabecalho = Row::new(vec![
        "endereço",
        "cliente",
        "progresso",
        "↓",
        "↑",
        "voo",
        "estado",
    ])
    .style(Style::new().fg(APAGADO).add_modifier(Modifier::BOLD));

    let linhas: Vec<Row> = snap
        .peers
        .iter()
        .map(|p| {
            let mut marcas = Vec::new();
            if p.snubbed {
                marcas.push(Span::styled("mudo ", Style::new().fg(RUIM)));
            }
            if p.choking_us {
                marcas.push(Span::styled("bloq ", Style::new().fg(ATENCAO)));
            } else {
                marcas.push(Span::styled("livre ", Style::new().fg(OK)));
            }
            if p.interested_in_us {
                marcas.push(Span::styled(
                    if p.we_choke { "quer " } else { "servindo " },
                    Style::new().fg(if p.we_choke { APAGADO } else { ACENTO }),
                ));
            }

            Row::new(vec![
                Cell::from(p.addr.to_string()),
                Cell::from(p.client.chars().take(18).collect::<String>()),
                Cell::from(format!("{:>5.1}%", p.progress * 100.0)),
                Cell::from(Span::styled(rate(p.download_rate), Style::new().fg(OK))),
                Cell::from(Span::styled(rate(p.upload_rate), Style::new().fg(ACENTO))),
                Cell::from(p.in_flight.to_string()),
                Cell::from(Line::from(marcas)),
            ])
        })
        .collect();

    let larguras = [
        Constraint::Length(22),
        Constraint::Length(19),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Min(18),
    ];
    frame.render_widget(
        Table::new(linhas, larguras)
            .header(cabecalho)
            .block(moldura(format!(
                " {} peers conectados  ·  enxame: {} seeds / {} leechers ",
                snap.connected, snap.seeds_in_swarm, snap.peers_in_swarm
            ))),
        area,
    );
}

fn desenhar_arquivos(frame: &mut Frame, area: Rect, snap: &Snapshot, selecao: usize) {
    let selecao = selecao.min(snap.files.len().saturating_sub(1));
    let itens: Vec<ListItem> = snap
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let fracao = if f.length == 0 {
                1.0
            } else {
                f.downloaded as f64 / f.length as f64
            };
            let (marca_prio, cor_prio) = match f.priority {
                Priority::Skip => ("pular ", APAGADO),
                Priority::Low => ("baixa ", APAGADO),
                Priority::Normal => ("normal", Color::Reset),
                Priority::High => ("ALTA  ", ATENCAO),
            };
            let selecionado = i == selecao;

            let barra_largura = 20usize;
            let cheias = (fracao * barra_largura as f64).round() as usize;
            let barra: String = "█".repeat(cheias) + &"░".repeat(barra_largura - cheias);

            let estilo_nome = if f.priority == Priority::Skip {
                Style::new().fg(APAGADO).add_modifier(Modifier::CROSSED_OUT)
            } else if selecionado {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    if selecionado { "▶ " } else { "  " },
                    Style::new().fg(ACENTO),
                ),
                Span::styled(marca_prio.to_string(), Style::new().fg(cor_prio)),
                Span::raw("  "),
                Span::styled(
                    barra,
                    Style::new().fg(if fracao >= 1.0 { OK } else { ACENTO }),
                ),
                Span::raw(format!(" {:>5.1}%  ", fracao * 100.0)),
                Span::styled(
                    format!("{:>10}  ", bytes(f.length)),
                    Style::new().fg(APAGADO),
                ),
                Span::styled(f.path.display().to_string(), estilo_nome),
            ]))
        })
        .collect();

    let vazio = itens.is_empty();
    frame.render_widget(
        List::new(itens).block(moldura(format!(" {} arquivos ", snap.files.len()))),
        area,
    );
    if vazio {
        let bloco = moldura(" arquivos ");
        let interno = bloco.inner(area);
        frame.render_widget(bloco, area);
        frame.render_widget(
            Paragraph::new("aguardando metadata…").style(Style::new().fg(APAGADO)),
            interno,
        );
    }
}

fn desenhar_trackers(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let [lista, info] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(7)]).areas(area);

    let linhas: Vec<Row> = snap
        .trackers
        .iter()
        .map(|t| {
            let (rotulo, cor) = match t.state {
                crate::tracker::TrackerState::Ok => ("ok", OK),
                crate::tracker::TrackerState::Consultando => ("consultando", ACENTO),
                crate::tracker::TrackerState::Falhou => ("falhou", RUIM),
                crate::tracker::TrackerState::Aguardando => ("aguardando", APAGADO),
                crate::tracker::TrackerState::Desligado => ("desligado", APAGADO),
            };
            Row::new(vec![
                Cell::from(t.url.chars().take(48).collect::<String>()),
                Cell::from(Span::styled(rotulo, Style::new().fg(cor))),
                Cell::from(t.seeders.to_string()),
                Cell::from(t.leechers.to_string()),
                Cell::from(t.peers.to_string()),
                Cell::from(if t.next_in > 0 {
                    format!("{}s", t.next_in)
                } else {
                    "—".to_string()
                }),
                Cell::from(
                    t.message
                        .clone()
                        .unwrap_or_default()
                        .chars()
                        .take(40)
                        .collect::<String>(),
                ),
            ])
        })
        .collect();

    let larguras = [
        Constraint::Min(30),
        Constraint::Length(12),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Min(20),
    ];
    frame.render_widget(
        Table::new(linhas, larguras)
            .header(
                Row::new(vec!["tracker", "estado", "seeds", "leechers", "peers", "próx", "aviso"])
                    .style(Style::new().fg(APAGADO).add_modifier(Modifier::BOLD)),
            )
            .block(moldura(" trackers ")),
        lista,
    );

    let texto = vec![
        Line::from(vec![
            Span::styled("info_hash  ", Style::new().fg(APAGADO)),
            Span::raw(snap.info_hash.clone()),
        ]),
        Line::from(vec![
            Span::styled("DHT        ", Style::new().fg(APAGADO)),
            Span::raw(if snap.dht_nodes > 0 {
                format!(
                    "{} nós ({} ativos) · {} consultas enviadas",
                    snap.dht_nodes, snap.dht_good, snap.dht_queries
                )
            } else {
                "desligado".to_string()
            }),
        ]),
        Line::from(vec![
            Span::styled("escutando  ", Style::new().fg(APAGADO)),
            Span::raw(format!("porta {}", snap.listen_port)),
        ]),
        Line::from(vec![
            Span::styled("tamanho    ", Style::new().fg(APAGADO)),
            Span::raw(format!(
                "{} em {} peças",
                bytes(snap.total_size),
                snap.total_pieces
            )),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(texto).wrap(Wrap { trim: true }).block(moldura(" torrent ")),
        info,
    );
}

fn desenhar_rodape(frame: &mut Frame, area: Rect, aba: Aba) {
    let mut atalhos: Vec<(&str, &str)> = vec![
        ("q", "sair"),
        ("tab", "aba"),
        ("a", "adicionar"),
        ("p", "pausar"),
        ("s", "sequencial"),
        ("r", "verificar"),
        ("n", "próximo"),
    ];
    if aba == Aba::Arquivos {
        atalhos.push(("espaço", "pular/incluir"));
        atalhos.push(("+/-", "prioridade"));
    }

    let mut spans = Vec::new();
    for (tecla, acao) in atalhos {
        spans.push(Span::styled(
            format!(" {tecla} "),
            Style::new().fg(Color::Black).bg(ACENTO),
        ));
        spans.push(Span::styled(
            format!(" {acao}   "),
            Style::new().fg(APAGADO),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{FileInfo, PeerInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn snapshot_de_teste() -> Snapshot {
        Snapshot {
            name: "pacote-de-teste".to_string(),
            info_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            state_label: "baixando".to_string(),
            have_pieces: 8,
            total_pieces: 16,
            pieces: vec![0xff, 0x00],
            partials: vec![(8, 0.5)],
            downloaded: 8 * 1024 * 1024,
            uploaded: 2 * 1024 * 1024,
            wanted: 16 * 1024 * 1024,
            total_size: 16 * 1024 * 1024,
            download_rate: 3.0 * 1024.0 * 1024.0,
            upload_rate: 512.0 * 1024.0,
            eta: Some(125),
            connected: 2,
            candidates: 40,
            dht_nodes: 120,
            down_history: vec![1.0, 5.0, 3.0],
            up_history: vec![0.5, 1.0],
            peers: vec![PeerInfo {
                addr: "10.0.0.7:51413".parse().unwrap(),
                client: "outro-cliente/1.0".to_string(),
                progress: 0.75,
                download_rate: 2.0 * 1024.0 * 1024.0,
                upload_rate: 0.0,
                choking_us: false,
                interested_in_us: true,
                we_choke: false,
                in_flight: 12,
                snubbed: false,
            }],
            files: vec![
                FileInfo {
                    path: "pacote/video.mkv".into(),
                    length: 12 * 1024 * 1024,
                    downloaded: 6 * 1024 * 1024,
                    priority: Priority::High,
                },
                FileInfo {
                    path: "pacote/extra.nfo".into(),
                    length: 4 * 1024 * 1024,
                    downloaded: 0,
                    priority: Priority::Skip,
                },
            ],
            ..Default::default()
        }
    }

    /// Desenha uma aba em um terminal falso e devolve o texto da tela.
    fn render(aba: Aba, snaps: &[Snapshot]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        let app = App {
            aba,
            ..App::default()
        };
        terminal
            .draw(|frame| desenhar(frame, &app, snaps))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let largura = buffer.area.width as usize;
        buffer
            .content()
            .chunks(largura)
            .map(|linha| linha.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn desenha_a_aba_geral() {
        let tela = render(Aba::Geral, &[snapshot_de_teste()]);
        assert!(tela.contains("pacote-de-teste"), "título do torrent");
        assert!(tela.contains("baixando"));
        assert!(tela.contains("50.0%"), "8 de 16 MiB");
        assert!(tela.contains("3.00 MiB/s"), "taxa de download");
        assert!(tela.contains("2m 5s"), "tempo restante");
        assert!(tela.contains("2 peers"));
        assert!(tela.contains("DHT 120 nós"));
        assert!(tela.contains('█'), "mapa de peças com blocos cheios");
    }

    #[test]
    fn desenha_as_demais_abas() {
        let snaps = [snapshot_de_teste()];

        let peers = render(Aba::Peers, &snaps);
        assert!(peers.contains("10.0.0.7:51413"));
        assert!(peers.contains("outro-cliente"));
        assert!(peers.contains("75.0%"));
        assert!(peers.contains("livre"), "peer não está nos estrangulando");

        let arquivos = render(Aba::Arquivos, &snaps);
        assert!(arquivos.contains("video.mkv"));
        assert!(arquivos.contains("ALTA"), "prioridade do arquivo");
        assert!(arquivos.contains("pular"), "arquivo marcado para pular");
        assert!(arquivos.contains("▶"), "cursor na seleção");

        let trackers = render(Aba::Trackers, &snaps);
        assert!(trackers.contains("0123456789abcdef"), "info_hash");
        assert!(trackers.contains("16 peças"));
    }

    #[test]
    fn tela_de_abertura_ensina_o_que_fazer() {
        let tela = render(Aba::Geral, &[]);
        assert!(tela.contains("cliente bittorrent"), "logo e subtítulo");
        assert!(tela.contains("cole um magnet link"));
        assert!(tela.contains("arraste para cima do torrra.exe"));
    }

    #[test]
    #[ignore]
    fn olhar_telas() {
        let mut terminal = Terminal::new(TestBackend::new(78, 22)).unwrap();
        let mostrar = |terminal: &mut Terminal<TestBackend>, app: &App, titulo: &str| {
            terminal.draw(|frame| desenhar(frame, app, &[])).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let largura = buffer.area.width as usize;
            println!("\n=== {titulo} ===");
            for linha in buffer.content().chunks(largura) {
                println!("{}", linha.iter().map(|c| c.symbol()).collect::<String>());
            }
        };
        mostrar(&mut terminal, &App::default(), "abertura");
        mostrar(
            &mut terminal,
            &App {
                avisos: vec![
                    "`quebrado.torrent` não é um .torrent válido: invalid digit found in string"
                        .into(),
                    "filme.torrent: The system cannot find the file specified. (os error 2)".into(),
                ],
                ..App::default()
            },
            "avisos",
        );
    }

    #[test]
    fn arquivo_recusado_aparece_na_tela() {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let app = App {
            avisos: vec!["quebrado.torrent: bencode inválido".into()],
            ..App::default()
        };
        terminal.draw(|frame| desenhar(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let largura = buffer.area.width as usize;
        let tela: String = buffer
            .content()
            .chunks(largura)
            .map(|linha| linha.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();

        assert!(tela.contains("não consegui abrir 1 arquivo"), "{tela}");
        assert!(tela.contains("quebrado.torrent"), "o nome do culpado");
        assert!(tela.contains("bencode inválido"), "e o motivo");
        assert!(tela.contains("qualquer tecla continua"));
    }

    /// Vinte arquivos ruins não podem estourar a caixa nem esconder o resto.
    #[test]
    fn muitos_avisos_cabem_na_caixa() {
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let app = App {
            avisos: (0..20).map(|i| format!("arquivo-{i}.torrent: ruim")).collect(),
            ..App::default()
        };
        terminal.draw(|frame| desenhar(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let largura = buffer.area.width as usize;
        let tela: String = buffer
            .content()
            .chunks(largura)
            .map(|linha| linha.iter().map(|c| c.symbol()).collect::<String>() + "\n")
            .collect();

        assert!(tela.contains("não consegui abrir 20 arquivos"), "{tela}");
        assert!(tela.contains("… e mais"), "diz quantos ficaram de fora");
        // O rodapé é o que ensina a sair da caixa: não pode ser o cortado.
        assert!(tela.contains("qualquer tecla continua"), "{tela}");
    }

    #[test]
    fn quebra_o_texto_em_palavras() {
        assert_eq!(quebrar("um dois tres", 20), ["um dois tres"]);
        assert_eq!(quebrar("um dois tres", 8), ["um dois", "tres"]);
        // Palavra sozinha maior que a linha: corta, não trava.
        assert_eq!(quebrar("magnetíssimo", 5), ["magne"]);
        assert_eq!(quebrar("", 10), Vec::<String>::new());
        assert_eq!(quebrar("qualquer", 0), Vec::<String>::new());
        // Acentos contam como um caractere.
        assert_eq!(quebrar("ação ção", 4), ["ação", "ção"]);
    }

    /// Mesmo sem espaço para o logo, a tela precisa dizer o que fazer.
    #[test]
    fn tela_de_abertura_encolhe_sem_quebrar() {
        let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
        let app = App::default();
        terminal.draw(|frame| desenhar(frame, &app, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let texto: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(texto.contains("arraste"), "instrução sobrevive ao aperto");
    }

    fn render_com_entrada(entrada: Entrada, snaps: &[Snapshot]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let app = App {
            entrada: Some(entrada),
            ..App::default()
        };
        terminal.draw(|frame| desenhar(frame, &app, snaps)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let largura = buffer.area.width as usize;
        buffer
            .content()
            .chunks(largura)
            .map(|l| l.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn caixa_de_entrada_mostra_texto_e_erro() {
        let tela = render_com_entrada(
            Entrada {
                texto: "magnet:?xt=urn:btih:abc".to_string(),
                erro: None,
            },
            &[],
        );
        assert!(tela.contains("adicionar torrent"), "título da caixa");
        assert!(tela.contains("magnet:?xt=urn:btih:abc"), "texto digitado");
        assert!(tela.contains("enter confirma"));

        let com_erro = render_com_entrada(
            Entrada {
                texto: "coisa.torrent".to_string(),
                erro: Some("não consegui abrir".to_string()),
            },
            &[],
        );
        assert!(com_erro.contains("não consegui abrir"));
    }

    /// A caixa flutua por cima da tela de um torrent em andamento.
    #[test]
    fn caixa_aparece_sobre_o_download() {
        let tela = render_com_entrada(
            Entrada {
                texto: String::new(),
                erro: None,
            },
            &[snapshot_de_teste()],
        );
        assert!(tela.contains("pacote-de-teste"), "o torrent segue atrás");
        assert!(tela.contains("adicionar torrent"), "e a caixa por cima");
    }

    /// Texto maior que a caixa mostra o fim, que é onde está o cursor.
    #[test]
    fn entrada_longa_rola_para_o_fim() {
        let longo = format!("magnet:?xt=urn:btih:{}FIM", "0".repeat(200));
        let tela = render_com_entrada(
            Entrada {
                texto: longo,
                erro: None,
            },
            &[],
        );
        assert!(tela.contains("FIM"), "o fim do texto fica visível");
    }

    #[test]
    fn desenha_em_terminal_apertado() {
        // Terminal pequeno não pode causar pânico por área negativa.
        let mut terminal = Terminal::new(TestBackend::new(24, 8)).unwrap();
        let app = App::default();
        let snaps = [snapshot_de_teste()];
        terminal
            .draw(|frame| desenhar(frame, &app, &snaps))
            .expect("desenho não pode falhar em tela pequena");
    }

    #[test]
    fn le_bits_do_bitfield() {
        let bits = [0b1010_0000u8, 0b0000_0001];
        assert!(bit_ligado(&bits, 0));
        assert!(!bit_ligado(&bits, 1));
        assert!(bit_ligado(&bits, 2));
        assert!(bit_ligado(&bits, 15));
        assert!(!bit_ligado(&bits, 99), "fora do vetor não estoura");
    }

    #[test]
    fn mapa_condensa_pecas_em_celulas() {
        let mut snap = Snapshot {
            total_pieces: 32,
            // Primeiras 8 peças completas.
            pieces: vec![0xff, 0x00, 0x00, 0x00],
            ..Default::default()
        };
        // Uma área de 4x1 significa 8 peças por célula.
        let area = Rect::new(0, 0, 4, 1);
        let p = mapa_de_pecas(&snap, area);
        let texto = format!("{p:?}");
        assert!(texto.contains('█'), "bloco cheio para as peças completas");
        assert!(texto.contains('·'), "vazio para o resto");

        snap.total_pieces = 0;
        let vazio = mapa_de_pecas(&snap, area);
        assert!(format!("{vazio:?}").contains("metadata"));
    }
}
