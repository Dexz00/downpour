//! Escolha de peças e blocos.
//!
//! Três decisões determinam a velocidade real do download:
//!
//! * **Rarest-first** — buscar primeiro as peças que poucos peers têm mantém o
//!   enxame saudável e evita ficar preso no fim esperando a única peça rara.
//! * **Fechar peças em progresso** — antes de abrir uma peça nova, terminar as
//!   que já começaram. Peça incompleta é banda gasta que ainda não conta.
//! * **Endgame** — no fim do download, pedir os últimos blocos a vários peers
//!   ao mesmo tempo, para um peer lento não segurar tudo.

use crate::bitfield::Bitfield;
use crate::metainfo::Info;
use rand::RngExt;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Tamanho de bloco padrão do protocolo. Praticamente todo cliente recusa
/// requisições maiores que 16 KiB.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Memória máxima dedicada a peças parcialmente baixadas.
const MAX_PARTIAL_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    pub piece: u32,
    pub offset: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickMode {
    /// Padrão: prioriza as peças mais raras do enxame.
    #[default]
    Rarest,
    /// Ordem linear, para assistir/ouvir enquanto baixa.
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Skip = 0,
    Low = 1,
    #[default]
    Normal = 2,
    High = 3,
}

/// O que aconteceu com um bloco recebido.
#[derive(Debug, Default)]
pub struct Received {
    /// Peers que também tinham este bloco pedido (endgame) — mande `cancel`.
    pub cancel_from: Vec<u32>,
    /// Preenchido quando o bloco fechou a peça; pronta para verificação.
    pub piece_data: Option<Vec<u8>>,
    /// `false` quando o bloco era duplicado ou não fazia parte de um pedido.
    pub wanted: bool,
}

#[derive(Debug)]
enum BlockState {
    Pending,
    Requested { by: Vec<u32>, since: Instant },
    Done,
}

struct Partial {
    blocks: Vec<BlockState>,
    buffer: Vec<u8>,
    done: usize,
}

impl Partial {
    fn new(size: u32) -> Partial {
        let blocks = size.div_ceil(BLOCK_SIZE) as usize;
        Partial {
            blocks: (0..blocks).map(|_| BlockState::Pending).collect(),
            buffer: vec![0; size as usize],
            done: 0,
        }
    }

    fn is_complete(&self) -> bool {
        self.done == self.blocks.len()
    }
}

// -- índice de raridade ----------------------------------------------------

/// Peças com disponibilidade acima disso caem todas no mesmo balde: a essa
/// altura a diferença entre "40 peers têm" e "50 peers têm" não muda nada.
const AVAIL_BUCKETS: usize = 33;
const RANKS: usize = 3 * AVAIL_BUCKETS;

/// Peças desejadas agrupadas por (prioridade, disponibilidade).
///
/// Amostragem aleatória sobre todas as peças parece razoável, mas erra o alvo:
/// com 48 sorteios entre 64 peças a peça rara escapa quase metade das vezes, e
/// a proporção só piora em torrents grandes. Os baldes dão a escolha exata sem
/// varrer nada — o balde de menor rank é, por construção, o mais raro.
struct RarityIndex {
    buckets: Vec<Vec<u32>>,
    at: HashMap<u32, (usize, usize)>,
}

impl RarityIndex {
    fn new() -> RarityIndex {
        RarityIndex {
            buckets: vec![Vec::new(); RANKS],
            at: HashMap::new(),
        }
    }

    fn rank_of(prio: Priority, avail: u32) -> Option<usize> {
        let faixa = match prio {
            Priority::High => 0,
            Priority::Normal => 1,
            Priority::Low => 2,
            Priority::Skip => return None,
        };
        Some(faixa * AVAIL_BUCKETS + (avail as usize).min(AVAIL_BUCKETS - 1))
    }

    fn len(&self) -> usize {
        self.at.len()
    }

    fn contains(&self, piece: u32) -> bool {
        self.at.contains_key(&piece)
    }

    fn insert(&mut self, piece: u32, rank: usize) {
        if self.at.contains_key(&piece) {
            self.relocate(piece, rank);
            return;
        }
        let bucket = &mut self.buckets[rank];
        self.at.insert(piece, (rank, bucket.len()));
        bucket.push(piece);
    }

    fn remove(&mut self, piece: u32) -> bool {
        let Some((rank, pos)) = self.at.remove(&piece) else {
            return false;
        };
        let bucket = &mut self.buckets[rank];
        bucket.swap_remove(pos);
        // O swap trouxe outra peça para `pos`; seu índice precisa acompanhar.
        if let Some(movida) = bucket.get(pos).copied() {
            self.at.insert(movida, (rank, pos));
        }
        true
    }

    fn relocate(&mut self, piece: u32, rank: usize) {
        if self.at.get(&piece).is_some_and(|(r, _)| *r == rank) {
            return;
        }
        if self.remove(piece) {
            self.insert(piece, rank);
        }
    }

    fn clear(&mut self) {
        for b in &mut self.buckets {
            b.clear();
        }
        self.at.clear();
    }

    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.buckets.iter().flatten().copied()
    }

    /// Peça mais rara que satisfaz `aceita`, começando pelo balde de menor rank.
    fn find(&self, aceita: impl Fn(u32) -> bool) -> Option<u32> {
        // Um balde pode ser enorme; varrer tudo a cada escolha custaria caro
        // quando o peer não tem nada dele. Fazemos uma passada limitada em
        // todos os baldes e só depois, se ninguém serviu, a varredura completa.
        const LIMITE: usize = 512;
        let mut rng = rand::rng();

        for bucket in self.buckets.iter().filter(|b| !b.is_empty()) {
            // Começar em um ponto aleatório evita que todos os peers ataquem a
            // mesma peça e desperdicem pedidos duplicados.
            let inicio = rng.random_range(0..bucket.len());
            for k in 0..bucket.len().min(LIMITE) {
                let piece = bucket[(inicio + k) % bucket.len()];
                if aceita(piece) {
                    return Some(piece);
                }
            }
        }
        self.buckets
            .iter()
            .filter(|b| b.len() > LIMITE)
            .flat_map(|b| b.iter().copied())
            .find(|p| aceita(*p))
    }
}

// -- picker ----------------------------------------------------------------

pub struct Picker {
    piece_length: u32,
    last_piece_size: u32,
    num_pieces: usize,
    have: Bitfield,
    /// Quantos peers conectados anunciam cada peça.
    availability: Vec<u32>,
    priority: Vec<Priority>,
    file_priority: Vec<Priority>,
    index: RarityIndex,
    partials: HashMap<u32, Partial>,
    max_partials: usize,
    mode: PickMode,
    cursor: u32,
    endgame: bool,
    downloaded: u64,
}

impl Picker {
    pub fn new(info: &Info, have: Bitfield) -> Picker {
        let num_pieces = info.num_pieces();
        let max_partials = (MAX_PARTIAL_BYTES / info.piece_length.max(1) as u64).clamp(8, 1024)
            as usize;

        let mut picker = Picker {
            piece_length: info.piece_length,
            last_piece_size: info.piece_size(num_pieces.saturating_sub(1)),
            num_pieces,
            have,
            availability: vec![0; num_pieces],
            priority: vec![Priority::Normal; num_pieces],
            file_priority: vec![Priority::Normal; info.files.len()],
            index: RarityIndex::new(),
            partials: HashMap::new(),
            max_partials,
            mode: PickMode::default(),
            cursor: 0,
            endgame: false,
            downloaded: 0,
        };
        picker.downloaded = picker
            .have
            .iter_set()
            .map(|i| picker.piece_size(i as u32) as u64)
            .sum();
        picker.rebuild_index();
        picker
    }

    pub fn set_mode(&mut self, mode: PickMode) {
        self.mode = mode;
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn is_complete(&self) -> bool {
        self.index.len() == 0
    }

    pub fn in_endgame(&self) -> bool {
        self.endgame
    }

    /// Bytes confirmados em disco.
    pub fn downloaded(&self) -> u64 {
        self.downloaded
    }

    /// Bytes que ainda faltam, contando só o que está marcado para baixar.
    pub fn remaining(&self) -> u64 {
        self.index
            .iter()
            .map(|i| self.piece_size(i) as u64)
            .sum::<u64>()
            .saturating_sub(self.partial_bytes())
    }

    fn partial_bytes(&self) -> u64 {
        self.partials
            .values()
            .map(|p| p.done as u64 * BLOCK_SIZE as u64)
            .sum()
    }

    pub fn piece_size(&self, index: u32) -> u32 {
        if index as usize + 1 == self.num_pieces {
            self.last_piece_size
        } else {
            self.piece_length
        }
    }

    // -- disponibilidade ---------------------------------------------------

    pub fn peer_added(&mut self, bf: &Bitfield) {
        for i in bf.iter_set() {
            if i < self.num_pieces {
                self.availability[i] += 1;
                self.reindex(i as u32);
            }
        }
    }

    pub fn peer_removed(&mut self, bf: &Bitfield) {
        for i in bf.iter_set() {
            if i < self.num_pieces {
                self.availability[i] = self.availability[i].saturating_sub(1);
                self.reindex(i as u32);
            }
        }
    }

    pub fn peer_has(&mut self, piece: u32) {
        if (piece as usize) < self.num_pieces {
            self.availability[piece as usize] += 1;
            self.reindex(piece);
        }
    }

    #[cfg(test)]
    pub fn availability(&self, piece: u32) -> u32 {
        self.availability.get(piece as usize).copied().unwrap_or(0)
    }

    /// Nós temos esta peça? Usado para responder pedidos de upload.
    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.get(piece as usize)
    }

    fn reindex(&mut self, piece: u32) {
        if !self.index.contains(piece) {
            return;
        }
        let prio = self.priority[piece as usize];
        match RarityIndex::rank_of(prio, self.availability[piece as usize]) {
            Some(rank) => self.index.relocate(piece, rank),
            None => {
                self.index.remove(piece);
            }
        }
    }

    // -- prioridades -------------------------------------------------------

    /// Ajusta a prioridade de um arquivo, propagando para as peças que ele ocupa.
    pub fn set_file_priority(&mut self, info: &Info, file_index: usize, prio: Priority) {
        if let Some(slot) = self.file_priority.get_mut(file_index) {
            *slot = prio;
        }
        self.recompute_piece_priorities(info);
        self.rebuild_index();
    }

    pub fn file_priority(&self, file_index: usize) -> Priority {
        self.file_priority
            .get(file_index)
            .copied()
            .unwrap_or_default()
    }

    /// Peças na fronteira entre dois arquivos herdam a maior das prioridades —
    /// não dá para baixar meia peça, então pular uma delas pularia parte de um
    /// arquivo que o usuário quer.
    fn recompute_piece_priorities(&mut self, info: &Info) {
        self.priority.fill(Priority::Skip);
        for (i, f) in info.files.iter().enumerate() {
            if f.length == 0 {
                continue;
            }
            let prio = self.file_priority(i);
            let first = (f.offset / self.piece_length as u64) as usize;
            let last = (((f.end() - 1) / self.piece_length as u64) as usize)
                .min(self.num_pieces - 1);
            for slot in &mut self.priority[first..=last] {
                *slot = (*slot).max(prio);
            }
        }
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for i in 0..self.num_pieces {
            if self.have.get(i) {
                continue;
            }
            if let Some(rank) = RarityIndex::rank_of(self.priority[i], self.availability[i]) {
                self.index.insert(i as u32, rank);
            }
        }
        self.partials.retain(|p, _| self.index.contains(*p));
    }

    // -- escolha -----------------------------------------------------------

    /// Preenche `out` com até `want` blocos que este peer pode servir.
    pub fn pick(&mut self, peer: u32, peer_bf: &Bitfield, want: usize, out: &mut Vec<BlockRequest>) {
        if want == 0 {
            return;
        }
        // 1. Fechar peças já iniciadas que este peer tem.
        let mut abertas: Vec<u32> = self
            .partials
            .keys()
            .copied()
            .filter(|p| peer_bf.get(*p as usize))
            .collect();
        abertas.sort_unstable_by_key(|p| (std::cmp::Reverse(self.priority[*p as usize]), *p));
        for piece in abertas {
            if out.len() >= want {
                return;
            }
            self.take_pending(piece, peer, want, out);
        }

        // 2. Abrir peças novas enquanto couber.
        while out.len() < want && self.partials.len() < self.max_partials {
            let Some(piece) = self.choose_piece(peer_bf) else {
                break;
            };
            let size = self.piece_size(piece);
            self.partials
                .entry(piece)
                .or_insert_with(|| Partial::new(size));
            let antes = out.len();
            self.take_pending(piece, peer, want, out);
            if out.len() == antes {
                break;
            }
        }

        // 3. Endgame: repetir pedidos pendentes em outros peers.
        if out.is_empty() && self.should_endgame() {
            self.endgame = true;
            self.take_endgame(peer, peer_bf, want, out);
        }
    }

    /// O endgame só vale a pena quando o que resta cabe em poucos pedidos —
    /// antes disso, pedir em duplicidade só desperdiça banda.
    fn should_endgame(&self) -> bool {
        if self.index.len() == 0 {
            return false;
        }
        let restantes: usize = self
            .partials
            .values()
            .map(|p| p.blocks.len() - p.done)
            .sum();
        self.index.len() == self.partials.len() && restantes <= 64
    }

    fn take_pending(&mut self, piece: u32, peer: u32, want: usize, out: &mut Vec<BlockRequest>) {
        let piece_size = self.piece_size(piece);
        let Some(partial) = self.partials.get_mut(&piece) else {
            return;
        };
        for (i, state) in partial.blocks.iter_mut().enumerate() {
            if out.len() >= want {
                return;
            }
            if let BlockState::Pending = state {
                let offset = i as u32 * BLOCK_SIZE;
                *state = BlockState::Requested {
                    by: vec![peer],
                    since: Instant::now(),
                };
                out.push(BlockRequest {
                    piece,
                    offset,
                    length: BLOCK_SIZE.min(piece_size - offset),
                });
            }
        }
    }

    fn take_endgame(
        &mut self,
        peer: u32,
        peer_bf: &Bitfield,
        want: usize,
        out: &mut Vec<BlockRequest>,
    ) {
        let pieces: Vec<u32> = self
            .partials
            .keys()
            .copied()
            .filter(|p| peer_bf.get(*p as usize))
            .collect();
        for piece in pieces {
            let piece_size = self.piece_size(piece);
            let Some(partial) = self.partials.get_mut(&piece) else {
                continue;
            };
            for (i, state) in partial.blocks.iter_mut().enumerate() {
                if out.len() >= want {
                    return;
                }
                if let BlockState::Requested { by, .. } = state
                    && !by.contains(&peer)
                {
                    by.push(peer);
                    let offset = i as u32 * BLOCK_SIZE;
                    out.push(BlockRequest {
                        piece,
                        offset,
                        length: BLOCK_SIZE.min(piece_size - offset),
                    });
                }
            }
        }
    }

    /// Escolhe a próxima peça a abrir, conforme o modo ativo.
    fn choose_piece(&mut self, peer_bf: &Bitfield) -> Option<u32> {
        if self.mode == PickMode::Sequential {
            // Avança o cursor pelas peças já resolvidas e para na primeira que
            // ainda falta — mesmo que este peer não a tenha, outro pode ter.
            while (self.cursor as usize) < self.num_pieces && !self.index.contains(self.cursor) {
                self.cursor += 1;
            }
            let disponivel = (self.cursor as usize..self.num_pieces)
                .map(|p| p as u32)
                .take(64)
                .find(|p| self.is_candidate(*p, peer_bf));
            if disponivel.is_some() {
                return disponivel;
            }
        }
        self.index.find(|p| self.is_candidate(p, peer_bf))
    }

    fn is_candidate(&self, piece: u32, peer_bf: &Bitfield) -> bool {
        self.index.contains(piece)
            && !self.partials.contains_key(&piece)
            && peer_bf.get(piece as usize)
    }

    // -- recebimento -------------------------------------------------------

    pub fn received(&mut self, peer: u32, req: BlockRequest, data: &[u8]) -> Received {
        let mut resultado = Received::default();
        if !req.offset.is_multiple_of(BLOCK_SIZE) {
            return resultado;
        }
        let Some(partial) = self.partials.get_mut(&req.piece) else {
            return resultado;
        };
        let idx = (req.offset / BLOCK_SIZE) as usize;
        let Some(state) = partial.blocks.get_mut(idx) else {
            return resultado;
        };
        if matches!(state, BlockState::Done) {
            return resultado;
        }
        let fim = req.offset as usize + data.len();
        if fim > partial.buffer.len() {
            return resultado;
        }

        if let BlockState::Requested { by, .. } = state {
            resultado.cancel_from = by.iter().copied().filter(|p| *p != peer).collect();
        }
        partial.buffer[req.offset as usize..fim].copy_from_slice(data);
        *state = BlockState::Done;
        partial.done += 1;
        resultado.wanted = true;

        if partial.is_complete() {
            let partial = self.partials.remove(&req.piece).expect("acabou de existir");
            resultado.piece_data = Some(partial.buffer);
        }
        resultado
    }

    /// A peça passou no SHA-1: sai da lista de desejadas para sempre.
    pub fn piece_verified(&mut self, piece: u32) {
        if self.have.set(piece as usize) {
            self.downloaded += self.piece_size(piece) as u64;
        }
        self.index.remove(piece);
        self.partials.remove(&piece);
        if self.mode == PickMode::Sequential && piece == self.cursor {
            self.cursor += 1;
        }
    }

    /// A peça falhou no SHA-1: recomeça do zero.
    pub fn piece_failed(&mut self, piece: u32) {
        self.partials.remove(&piece);
    }

    /// Devolve ao pool tudo que este peer tinha pedido e não entregou.
    pub fn release_peer(&mut self, peer: u32) {
        for partial in self.partials.values_mut() {
            for state in &mut partial.blocks {
                if let BlockState::Requested { by, .. } = state {
                    by.retain(|p| *p != peer);
                    if by.is_empty() {
                        *state = BlockState::Pending;
                    }
                }
            }
        }
    }

    /// Blocos pedidos há tempo demais voltam a ficar disponíveis.
    ///
    /// Sem isso, um peer que aceita o pedido e nunca responde congela a peça
    /// até a desconexão — o jeito mais comum de um download "travar em 99%".
    pub fn sweep_timeouts(&mut self, limite: Duration) -> usize {
        let agora = Instant::now();
        let mut liberados = 0;
        for partial in self.partials.values_mut() {
            for state in &mut partial.blocks {
                if let BlockState::Requested { since, .. } = state
                    && agora.duration_since(*since) > limite
                {
                    *state = BlockState::Pending;
                    liberados += 1;
                }
            }
        }
        liberados
    }

    /// Peças abertas no momento, para exibição.
    pub fn partial_pieces(&self) -> Vec<(u32, f32)> {
        let mut v: Vec<(u32, f32)> = self
            .partials
            .iter()
            .map(|(p, partial)| (*p, partial.done as f32 / partial.blocks.len() as f32))
            .collect();
        v.sort_unstable_by_key(|(p, _)| *p);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{self, Value};
    use crate::metainfo::Metainfo;
    use sha1::{Digest, Sha1};

    fn info_de_teste(num_pieces: usize, piece_len: u32) -> Info {
        let total = num_pieces as i64 * piece_len as i64;
        let info = Value::dict([
            ("length", Value::Int(total)),
            ("name", Value::bytes("teste")),
            ("piece length", Value::Int(piece_len as i64)),
            ("pieces", Value::Bytes(vec![0u8; num_pieces * 20])),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        Metainfo::from_bytes(&raw).unwrap().info
    }

    fn bitfield_cheio(n: usize) -> Bitfield {
        let mut bf = Bitfield::new(n);
        bf.set_all();
        bf
    }

    #[test]
    fn pede_blocos_e_completa_peca() {
        let info = info_de_teste(4, BLOCK_SIZE * 2);
        let mut picker = Picker::new(&info, Bitfield::new(4));
        let peer_bf = bitfield_cheio(4);
        picker.peer_added(&peer_bf);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 2, &mut reqs);
        assert_eq!(reqs.len(), 2, "dois blocos por peça");
        assert_eq!(reqs[0].length, BLOCK_SIZE);
        assert_eq!(reqs[1].offset, BLOCK_SIZE);

        // Um pedido repetido não deve devolver os mesmos blocos.
        let mut outros = Vec::new();
        picker.pick(1, &peer_bf, 2, &mut outros);
        assert!(outros.iter().all(|r| !reqs.contains(r)));

        let dados = vec![7u8; BLOCK_SIZE as usize];
        assert!(picker.received(1, reqs[0], &dados).piece_data.is_none());
        let fim = picker.received(1, reqs[1], &dados);
        let completa = fim.piece_data.expect("peça deve fechar");
        assert_eq!(completa.len(), (BLOCK_SIZE * 2) as usize);
        assert!(completa.iter().all(|b| *b == 7));
    }

    #[test]
    fn ignora_bloco_duplicado_ou_estranho() {
        let info = info_de_teste(2, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(2));
        let peer_bf = bitfield_cheio(2);
        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);

        let dados = vec![1u8; BLOCK_SIZE as usize];
        assert!(picker.received(1, reqs[0], &dados).wanted);
        assert!(!picker.received(1, reqs[0], &dados).wanted, "duplicado");

        let intruso = BlockRequest {
            piece: 1,
            offset: 999,
            length: 16,
        };
        assert!(!picker.received(1, intruso, &dados).wanted, "offset torto");
    }

    #[test]
    fn prefere_pecas_raras() {
        let info = info_de_teste(64, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(64));

        // Todos os peers têm tudo, menos a peça 42, que só um tem.
        let mut comum = bitfield_cheio(64);
        comum.clear(42);
        for _ in 0..30 {
            picker.peer_added(&comum);
        }
        let raro = bitfield_cheio(64);
        picker.peer_added(&raro);

        for _ in 0..20 {
            let mut reqs = Vec::new();
            let mut fresh = Picker::new(&info, Bitfield::new(64));
            fresh.availability.copy_from_slice(&picker.availability);
            fresh.rebuild_index();
            fresh.pick(1, &raro, 1, &mut reqs);
            assert_eq!(reqs[0].piece, 42, "a peça mais rara vem sempre primeiro");
        }
    }

    #[test]
    fn raridade_acompanha_peers_que_saem() {
        let info = info_de_teste(8, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(8));

        let mut so_a_3 = Bitfield::new(8);
        so_a_3.set(3);
        let cheio = bitfield_cheio(8);
        picker.peer_added(&cheio);
        picker.peer_added(&so_a_3);
        assert_eq!(picker.availability(3), 2);

        // Com a peça 3 sendo a mais comum, ela não deve ser a escolhida.
        let mut reqs = Vec::new();
        picker.pick(1, &cheio, 1, &mut reqs);
        assert_ne!(reqs[0].piece, 3);

        picker.peer_removed(&cheio);
        assert_eq!(picker.availability(3), 1);
        assert_eq!(picker.availability(0), 0);
    }

    #[test]
    fn modo_sequencial_segue_a_ordem() {
        let info = info_de_teste(16, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(16));
        picker.set_mode(PickMode::Sequential);
        let peer_bf = bitfield_cheio(16);
        picker.peer_added(&peer_bf);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 3, &mut reqs);
        let mut pecas: Vec<u32> = reqs.iter().map(|r| r.piece).collect();
        pecas.sort_unstable();
        assert_eq!(pecas, vec![0, 1, 2]);
    }

    #[test]
    fn timeout_e_desconexao_liberam_blocos() {
        let info = info_de_teste(2, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(2));
        let peer_bf = bitfield_cheio(2);

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);
        let mut vazio = Vec::new();
        picker.pick(2, &peer_bf, 1, &mut vazio);
        assert_ne!(vazio[0], reqs[0], "peer 2 não repete o bloco do peer 1");

        picker.release_peer(1);
        let mut denovo = Vec::new();
        picker.pick(3, &peer_bf, 4, &mut denovo);
        assert!(denovo.contains(&reqs[0]), "bloco do peer 1 voltou ao pool");

        assert_eq!(picker.sweep_timeouts(Duration::ZERO), 2);
    }

    #[test]
    fn endgame_duplica_os_ultimos_blocos() {
        let info = info_de_teste(1, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(1));
        let peer_bf = bitfield_cheio(1);

        let mut lento = Vec::new();
        picker.pick(1, &peer_bf, 4, &mut lento);
        assert_eq!(lento.len(), 1);
        assert!(!picker.in_endgame());

        let mut rapido = Vec::new();
        picker.pick(2, &peer_bf, 4, &mut rapido);
        assert_eq!(rapido, lento, "no endgame o mesmo bloco é pedido de novo");
        assert!(picker.in_endgame());

        // Quem entregar primeiro manda cancelar no outro.
        let dados = vec![0u8; BLOCK_SIZE as usize];
        let r = picker.received(2, rapido[0], &dados);
        assert_eq!(r.cancel_from, vec![1]);
        assert!(r.piece_data.is_some());
    }

    #[test]
    fn peca_verificada_sai_da_fila() {
        let info = info_de_teste(3, BLOCK_SIZE);
        let mut picker = Picker::new(&info, Bitfield::new(3));
        assert_eq!(picker.remaining(), 3 * BLOCK_SIZE as u64);
        picker.piece_verified(1);
        assert!(picker.has_piece(1));
        assert_eq!(picker.downloaded(), BLOCK_SIZE as u64);
        assert_eq!(picker.remaining(), 2 * BLOCK_SIZE as u64);

        picker.piece_verified(0);
        picker.piece_verified(2);
        assert!(picker.is_complete());
    }

    #[test]
    fn arquivo_com_skip_sai_do_download() {
        let arquivo = |len: i64, nome: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(nome)])),
            ])
        };
        let piece_len = BLOCK_SIZE as i64;
        let mut pieces = Vec::new();
        pieces.extend_from_slice(&Sha1::digest(vec![0u8; piece_len as usize]));
        pieces.extend_from_slice(&Sha1::digest(vec![0u8; piece_len as usize]));
        let info = Value::dict([
            (
                "files",
                Value::List(vec![arquivo(piece_len, "a"), arquivo(piece_len, "b")]),
            ),
            ("name", Value::bytes("dois")),
            ("piece length", Value::Int(piece_len)),
            ("pieces", Value::Bytes(pieces)),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        let info = Metainfo::from_bytes(&raw).unwrap().info;

        let mut picker = Picker::new(&info, Bitfield::new(2));
        picker.set_file_priority(&info, 1, Priority::Skip);
        assert_eq!(picker.remaining(), piece_len as u64, "só o arquivo desejado conta");

        let peer_bf = bitfield_cheio(2);
        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 8, &mut reqs);
        assert!(
            reqs.iter().all(|r| r.piece == 0),
            "peça do arquivo pulado não é pedida"
        );
        picker.piece_verified(0);
        assert!(picker.is_complete(), "só o arquivo desejado conta");
    }

    #[test]
    fn prioridade_alta_fura_a_fila() {
        let arquivo = |len: i64, nome: &str| {
            Value::dict([
                ("length", Value::Int(len)),
                ("path", Value::List(vec![Value::bytes(nome)])),
            ])
        };
        let piece_len = BLOCK_SIZE as i64;
        let info = Value::dict([
            (
                "files",
                Value::List((0..4).map(|i| arquivo(piece_len, &format!("f{i}"))).collect()),
            ),
            ("name", Value::bytes("quatro")),
            ("piece length", Value::Int(piece_len)),
            ("pieces", Value::Bytes(vec![0u8; 4 * 20])),
        ]);
        let raw = bencode::encode(&Value::dict([("info", info)]));
        let info = Metainfo::from_bytes(&raw).unwrap().info;

        let mut picker = Picker::new(&info, Bitfield::new(4));
        let peer_bf = bitfield_cheio(4);
        picker.peer_added(&peer_bf);
        // O último arquivo é o mais comum do enxame, mas o usuário o quer já.
        picker.set_file_priority(&info, 3, Priority::High);
        for _ in 0..10 {
            picker.peer_has(3);
        }

        let mut reqs = Vec::new();
        picker.pick(1, &peer_bf, 1, &mut reqs);
        assert_eq!(reqs[0].piece, 3, "prioridade vence raridade");
    }
}
