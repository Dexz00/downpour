# torrra

Cliente BitTorrent de terminal escrito do zero em Rust — protocolo, DHT,
seleção de peças e interface, sem depender de nenhuma biblioteca de torrent.

Lê arquivos `.torrent` e magnet links.

```
torrra ubuntu.torrent -o ~/Downloads
torrra "magnet:?xt=urn:btih:..." -o ~/Downloads
```

## O que está implementado

| Parte | Especificação |
|---|---|
| Protocolo base, bencode, tracker HTTP | BEP-3 |
| Peers compactos do tracker | BEP-23 |
| DHT (busca e resposta a consultas) | BEP-5 |
| Fast extension | BEP-6 |
| Protocolo de extensões | BEP-10 |
| Metadata por magnet (`ut_metadata`) | BEP-9 |
| Troca de peers (`ut_pex`), envio e recepção | BEP-11 |
| Múltiplos trackers em tiers | BEP-12 |
| Tracker UDP | BEP-15 |
| Flag `private` | BEP-27 |

Fora isso: rarest-first, endgame, escolha sequencial para assistir enquanto
baixa, prioridade por arquivo, resume automático, limites de banda e
verificação SHA-1 de toda peça antes de gravar.

## Compilar

```
cargo build --release
```

O binário sai em `target/release/torrra`. No Windows ele já sai com o ícone e o
bloco de versão embutidos — um arquivo só, sem nada em volta para distribuir.

## No Windows: arrastar e soltar

O próprio `torrra.exe` é o alvo:

- **arraste** um ou vários `.torrent` para cima dele — baixa todos na mesma
  sessão;
- **clique duas vezes** — abre a interface vazia, onde se cola um magnet com
  `Ctrl+V` (a caixa abre sozinha) ou com a tecla `a`.

Aberto assim, o destino é a sua pasta **Downloads** — e não o diretório do
programa, que é o que o Explorer entrega. Pelo terminal nada muda: continua
sendo o diretório atual.

Para fixar outro destino, crie um `destino.txt` ao lado do `.exe` com o caminho
em uma linha. A ordem de precedência é `-o`, depois `destino.txt`, depois
Downloads.

## Uso

```
torrra <TORRENT|MAGNET>... [opções]

  -o, --saida <DIR>          diretório de destino (padrão: Downloads quando
                             aberto pelo gerenciador, senão o diretório atual)
  -p, --porta <N>            porta TCP e UDP (padrão: 6881)
  -s, --sequencial           baixa em ordem, para assistir enquanto baixa
      --max-peers <N>        conexões por torrent (padrão: 100)
      --slots-upload <N>     peers atendidos ao mesmo tempo (padrão: 6)
      --limite-download <T>  ex.: 2M, 500k
      --limite-upload <T>
      --sem-dht              só trackers
      --sem-seed             sai ao concluir, em vez de semear
      --simples              sem interface, uma linha de progresso por segundo
      --info <TORRENT>       mostra o conteúdo do arquivo e sai
```

### Interface

| Tecla | Ação |
|---|---|
| `Ctrl+V` | colar um magnet — a caixa abre sozinha |
| `a` | adicionar torrent (digitar magnet ou caminho) |
| `1`–`4`, `Tab` | trocar de aba |
| `↑` `↓` / `j` `k` | mover a seleção |
| `p` | pausar / retomar |
| `s` | alternar modo sequencial |
| `r` | reverificar os arquivos em disco |
| `n` | próximo torrent |
| `espaço` | (aba Arquivos) incluir ou pular o arquivo |
| `+` `-` | (aba Arquivos) prioridade |
| `q` | sair |

### Diagnóstico

`TORRRA_LOG` escreve detalhes em stderr, filtrando por assunto:

```
TORRRA_LOG=peer,dht torrra x.torrent 2> log.txt
```

Assuntos: `peer`, `peers`, `dht`, `estado`. `1` liga todos.

## Como está organizado

```
bencode      codificação bencode, preservando os bytes crus do `info`
metainfo     leitura do .torrent; saneamento de caminhos
magnet       magnet links (hex e base32)
id           identificadores de 20 bytes e distância XOR
bitfield     bitfield de peças no layout do protocolo
compact      endereços compactos (tracker, DHT, PEX)
storage      I/O posicional mapeando peças para múltiplos arquivos
picker       escolha de peças e blocos
peer/        wire protocol, extensões, ut_metadata, ut_pex
tracker/     announce HTTP e UDP, tiers, backoff
dht/         KRPC, tabela de roteamento, busca iterativa
engine       o laço que decide tudo
session      porta de escuta, DHT compartilhado, lista de torrents
plataforma   console, pasta de destino e janela quando não há terminal
ui           interface de terminal
build.rs     ícone e bloco de versão do executável no Windows
assets/      a logo e o script que a converte em torrra.ico
```

O `.ico` é versionado; ele só precisa ser regerado quando a logo mudar:

```
python assets/gerar-icone.py
```

O desenho é de ator: **um** laço decide, e todo o resto são tasks que só
executam. Cada peer é uma task que fala protocolo e reporta eventos; o disco
roda em `spawn_blocking`; a interface lê um `Snapshot` e nunca toca no estado
do engine. Não há estado compartilhado entre peers, então não há contenção nem
ordem de lock a respeitar.

## Decisões que valem explicação

**Rarest-first com baldes, não amostragem.** Sortear algumas peças e pegar a
mais rara entre elas parece bom o suficiente, e não é: com 48 sorteios entre 64
peças, a peça rara escapa quase metade das vezes — e a proporção piora em
torrents grandes. As peças desejadas ficam agrupadas por (prioridade,
disponibilidade), e o balde de menor índice é, por construção, o mais raro.
Escolha exata, custo constante.

**A leitura do socket não fica dentro do `select!`.** `read_message` consome os
4 bytes de tamanho antes do corpo; cancelada entre os dois, esses bytes somem e
a mensagem seguinte é lida a partir do corpo. O stream dessincroniza e a
conexão morre anunciando "mensagem de 1,6 GB". A leitura roda em uma task
própria e chega ao laço por um canal, que é cancel-safe. Isso separou 55 KiB/s
de 84 MiB/s.

**Toda peça é conferida antes de tocar o disco.** O SHA-1 roda sobre o buffer
em memória; só depois a peça é gravada. Peça corrompida nunca chega ao arquivo.

**Caminhos do `.torrent` são saneados.** Um `.torrent` é um arquivo de
terceiros: sem saneamento, um caminho como `["..", "..", "autorun.inf"]`
escreveria fora do diretório de destino. Nomes reservados do Windows e espaços
ou pontos finais também são tratados, senão o caminho gravado divergiria do
caminho verificado.

**Discar é largo e com prazo curto.** A maior parte dos endereços que o DHT
devolve está morta ou atrás de NAT; o custo de tentar é espera, não banda. 60
tentativas simultâneas com 6 segundos de prazo acham os peers vivos muito mais
rápido do que uma fila estreita e paciente.

**Loopback passa no parsing, mas não é discável.** Decodificar não é decidir: um
peer em `127.0.0.1` vindo de um tracker remoto é inútil, mas o mesmo endereço
vindo de um nó DHT na própria máquina é legítimo. A regra de conexão fica em
`is_routable`, separada de `is_usable`.

**Os nós do DHT são salvos entre execuções.** Eles costumam continuar de pé, e
entram na tabela sem custo de rede — os roteadores públicos ficam como plano B.

**O ícone é montado sem o compilador de recursos.** Embutir um ícone no `.exe`
normalmente pede o `rc.exe` do SDK do Windows, que não vem com o Rust e quase
nunca está no PATH — o build quebraria em máquina limpa, e por um enfeite. O
`.res` é uma sequência de registros de cabeçalho fixo, então o `build.rs` o
escreve direto e entrega ao `link.exe`. Sem dependência de build, sem etapa
manual.

**Aberto pelo Explorer, o programa se comporta como aplicativo.** Ali não há
shell: o console nasce na página de código antiga e some junto com o processo.
Então o programa põe o console em UTF-8, adota a pasta Downloads em vez do
diretório atual — que o Explorer define como o do próprio executável — e segura
a janela quando termina em erro. `GetConsoleProcessList` diz qual é o caso: se
só existe um processo no console, ninguém nos chamou de um terminal. Pelo
terminal, nada disso muda.

**Arquivo recusado vira tela, não `stderr`.** Arrastar cinco `.torrent` e ter um
corrompido não pode derrubar os outros quatro nem sumir sem explicação: a
interface ocupa a tela toda, e sem terminal por trás não sobra onde ler a
mensagem depois. Os recusados aparecem em uma caixa que a primeira tecla
dispensa.

**Endereços entram em quarentena, não em lista negra.** Marcar cada endereço
visto como "já conhecido" parece a forma óbvia de evitar discar duas vezes para
o mesmo peer, e envenena o enxame: todo peer que cai fica banido para sempre.
Depois de pausar e retomar, o tracker devolve exatamente os mesmos endereços,
todos são descartados como repetidos e o download trava mostrando "baixando"
sem baixar nada. A fila usa quarentena por tempo — curta para quem chegou a
conversar, longa para quem nem respondeu ao handshake.

## Testes

```
cargo test
```

113 testes cobrindo bencode, parsing de `.torrent` e magnet, bitfield, picker
(rarest-first, endgame, timeouts, prioridades), fila de candidatos (quarentena,
ciclo pausar/retomar), storage (peças que cruzam arquivos, corrupção, limites),
wire protocol, extensões, trackers HTTP e UDP, DHT (KRPC ida e volta, tabela de
roteamento, tokens), escolha do destino e renderização da interface.

Há também dois testes de rede marcados como `#[ignore]`, que falam com um
enxame de verdade:

```
TORRENT=caminho.torrent cargo test announce_real   -- --ignored --nocapture
TORRENT=caminho.torrent cargo test ciclo_pausa_real -- --ignored --nocapture
```

E um terceiro ignorado que só imprime as telas, para conferir layout a olho:

```
cargo test olhar_telas -- --ignored --nocapture
```

## Validação

Baixado e conferido de ponta a ponta com o Ubuntu 24.04.4 (6,2 GB): pelo
`.torrent` e por magnet puro sem nenhum tracker, com o SHA256 do arquivo final
idêntico ao publicado em `releases.ubuntu.com`.

## Limitações conhecidas

- Só BitTorrent v1. Torrents exclusivamente v2 (BEP-52) são recusados com
  mensagem clara; híbridos funcionam pela parte v1.
- Sem web seeds (BEP-19): a URL aparece em `--info`, mas não é usada como fonte.
- Sem µTP, sem criptografia de conexão (MSE) e sem UPnP/NAT-PMP — sem
  redirecionamento de porta, só conexões de saída.
- Sem IPv6 nos trackers UDP.
