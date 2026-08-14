//! Tabela de roteamento Kademlia.
//!
//! 160 baldes, um por bit de prefixo comum com o nosso id. Cada balde guarda
//! até `K` nós e prefere os antigos: um nó que já está de pé há horas tem mais
//! chance de continuar de pé do que um recém-chegado.

use super::krpc::NodeInfo;
use crate::id::Id20;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Tamanho do balde, fixado em 8 pela BEP-5.
pub const K: usize = 8;

/// Sem notícias por esse tempo, o nó vira "duvidoso" e precisa de um ping.
const TEMPO_DUVIDOSO: Duration = Duration::from_secs(15 * 60);
/// Falhas seguidas até descartar o nó.
const MAX_FALHAS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Good,
    Questionable,
    Bad,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub info: NodeInfo,
    pub last_seen: Instant,
    pub failures: u32,
}

impl Node {
    fn new(info: NodeInfo) -> Node {
        Node {
            info,
            last_seen: Instant::now(),
            failures: 0,
        }
    }

    pub fn state(&self) -> NodeState {
        if self.failures >= MAX_FALHAS {
            NodeState::Bad
        } else if self.last_seen.elapsed() > TEMPO_DUVIDOSO {
            NodeState::Questionable
        } else {
            NodeState::Good
        }
    }
}

#[derive(Debug, Default)]
struct Bucket {
    nodes: Vec<Node>,
    last_changed: Option<Instant>,
}

pub struct RoutingTable {
    own_id: Id20,
    buckets: Vec<Bucket>,
}

impl RoutingTable {
    pub fn new(own_id: Id20) -> RoutingTable {
        RoutingTable {
            own_id,
            buckets: (0..160).map(|_| Bucket::default()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    pub fn count_good(&self) -> usize {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Good)
            .count()
    }

    fn bucket_for(&self, id: &Id20) -> usize {
        self.own_id.bucket_index(id).min(159)
    }

    /// Registra um nó visto na rede.
    ///
    /// Devolve `true` se ele entrou na tabela. Baldes cheios só abrem espaço
    /// descartando um nó ruim — trocar um nó estável por um desconhecido só
    /// facilitaria a vida de quem quer poluir a tabela.
    pub fn add(&mut self, info: NodeInfo) -> bool {
        if info.id == self.own_id || !crate::compact::is_usable(&info.addr) {
            return false;
        }
        let idx = self.bucket_for(&info.id);
        let bucket = &mut self.buckets[idx];

        if let Some(existente) = bucket.nodes.iter_mut().find(|n| n.info.id == info.id) {
            existente.info.addr = info.addr;
            existente.last_seen = Instant::now();
            existente.failures = 0;
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        if bucket.nodes.len() < K {
            bucket.nodes.push(Node::new(info));
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        if let Some(ruim) = bucket.nodes.iter().position(|n| n.state() == NodeState::Bad) {
            bucket.nodes[ruim] = Node::new(info);
            bucket.last_changed = Some(Instant::now());
            return true;
        }
        false
    }

    /// O nó respondeu: renova a validade dele.
    pub fn touch(&mut self, id: &Id20) {
        let idx = self.bucket_for(id);
        if let Some(node) = self.buckets[idx].nodes.iter_mut().find(|n| n.info.id == *id) {
            node.last_seen = Instant::now();
            node.failures = 0;
        }
    }

    /// O nó não respondeu; depois de algumas vezes ele é removido.
    pub fn mark_failed(&mut self, id: &Id20) {
        let idx = self.bucket_for(id);
        let bucket = &mut self.buckets[idx];
        if let Some(pos) = bucket.nodes.iter().position(|n| n.info.id == *id) {
            bucket.nodes[pos].failures += 1;
            if bucket.nodes[pos].state() == NodeState::Bad {
                bucket.nodes.remove(pos);
            }
        }
    }

    /// Os `n` nós mais próximos de `target`, do mais próximo ao mais distante.
    pub fn closest(&self, target: &Id20, n: usize) -> Vec<NodeInfo> {
        let mut candidatos: Vec<&Node> = self
            .buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|node| node.state() != NodeState::Bad)
            .collect();
        candidatos.sort_unstable_by_key(|node| node.info.id.distance(target));
        candidatos.into_iter().take(n).map(|node| node.info).collect()
    }

    /// Nós duvidosos que merecem um ping antes de serem descartados.
    pub fn questionable(&self) -> Vec<NodeInfo> {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Questionable)
            .map(|n| n.info)
            .collect()
    }

    /// Ids aleatórios dentro de baldes parados, para reanimá-los com um
    /// `find_node`. Sem isso a tabela envelhece e a busca vai perdendo alcance.
    pub fn refresh_targets(&self) -> Vec<Id20> {
        let mut alvos = Vec::new();
        for (i, bucket) in self.buckets.iter().enumerate() {
            let parado = bucket
                .last_changed
                .is_none_or(|t| t.elapsed() > TEMPO_DUVIDOSO);
            if parado && !bucket.nodes.is_empty() {
                alvos.push(random_in_bucket(&self.own_id, i));
            }
        }
        alvos
    }

    pub fn mark_bucket_refreshed(&mut self, target: &Id20) {
        let idx = self.bucket_for(target);
        self.buckets[idx].last_changed = Some(Instant::now());
    }

    /// Snapshot dos nós vivos, para gravar em disco e religar o DHT rápido na
    /// próxima execução.
    pub fn snapshot(&self) -> Vec<(Id20, SocketAddr)> {
        self.buckets
            .iter()
            .flat_map(|b| &b.nodes)
            .filter(|n| n.state() == NodeState::Good)
            .map(|n| (n.info.id, n.info.addr))
            .collect()
    }
}

/// Id aleatório que cai no balde `index` em relação a `own_id`: mesmos
/// `index` bits iniciais, bit seguinte invertido, resto aleatório.
fn random_in_bucket(own_id: &Id20, index: usize) -> Id20 {
    let mut bytes = Id20::random().0;
    for bit in 0..index.min(159) {
        let (byte, mask) = (bit / 8, 0x80 >> (bit % 8));
        bytes[byte] = (bytes[byte] & !mask) | (own_id.0[byte] & mask);
    }
    let (byte, mask) = (index / 8, 0x80 >> (index % 8));
    bytes[byte] ^= (bytes[byte] ^ !own_id.0[byte]) & mask;
    Id20(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no(id: Id20, porta: u16) -> NodeInfo {
        NodeInfo {
            id,
            addr: format!("1.2.3.4:{porta}").parse().unwrap(),
        }
    }

    #[test]
    fn balde_enche_e_para_de_aceitar() {
        let meu = Id20([0; 20]);
        let mut tabela = RoutingTable::new(meu);

        // Todos com o primeiro bit ligado caem no mesmo balde (índice 0).
        for i in 0..K {
            let mut id = [0u8; 20];
            id[0] = 0x80;
            id[19] = i as u8;
            assert!(tabela.add(no(Id20(id), 1000 + i as u16)));
        }
        assert_eq!(tabela.len(), K);

        let mut extra = [0u8; 20];
        extra[0] = 0x80;
        extra[19] = 99;
        assert!(!tabela.add(no(Id20(extra), 9999)), "balde cheio recusa");

        // Um id com prefixo diferente vai para outro balde e cabe.
        let mut outro = [0u8; 20];
        outro[0] = 0x40;
        assert!(tabela.add(no(Id20(outro), 8888)));
        assert_eq!(tabela.len(), K + 1);
    }

    #[test]
    fn no_ruim_abre_espaco() {
        let mut tabela = RoutingTable::new(Id20([0; 20]));
        let mut ids = Vec::new();
        for i in 0..K {
            let mut id = [0u8; 20];
            id[0] = 0x80;
            id[19] = i as u8;
            ids.push(Id20(id));
            tabela.add(no(Id20(id), 1000 + i as u16));
        }

        // Três falhas seguidas removem o nó da tabela.
        for _ in 0..MAX_FALHAS {
            tabela.mark_failed(&ids[0]);
        }
        assert_eq!(tabela.len(), K - 1);

        let mut novo = [0u8; 20];
        novo[0] = 0x80;
        novo[19] = 77;
        assert!(tabela.add(no(Id20(novo), 7777)));
    }

    #[test]
    fn nao_aceita_a_si_mesmo_nem_lixo() {
        let meu = Id20([9; 20]);
        let mut tabela = RoutingTable::new(meu);
        assert!(!tabela.add(no(meu, 6881)), "nós mesmos não entramos");
        assert!(
            !tabela.add(NodeInfo {
                id: Id20::random(),
                addr: "1.2.3.4:0".parse().unwrap(),
            }),
            "porta 0 não serve"
        );
        // Loopback entra: um nó DHT na própria máquina é legítimo.
        assert!(tabela.add(NodeInfo {
            id: Id20::random(),
            addr: "127.0.0.1:6881".parse().unwrap(),
        }));
    }

    #[test]
    fn closest_ordena_por_distancia() {
        let mut tabela = RoutingTable::new(Id20([0; 20]));
        let alvo = Id20([0xff; 20]);

        let perto = Id20([0xfe; 20]);
        let longe = Id20([0x01; 20]);
        tabela.add(no(longe, 1));
        tabela.add(no(perto, 2));

        let mais_proximos = tabela.closest(&alvo, 5);
        assert_eq!(mais_proximos.len(), 2);
        assert_eq!(mais_proximos[0].id, perto, "o mais próximo vem primeiro");
        assert_eq!(tabela.closest(&alvo, 1).len(), 1, "respeita o limite");
    }

    #[test]
    fn atualiza_endereco_de_no_conhecido() {
        let mut tabela = RoutingTable::new(Id20([0; 20]));
        let id = Id20([0x80; 20]);
        tabela.add(no(id, 1111));
        tabela.add(no(id, 2222));
        assert_eq!(tabela.len(), 1, "mesmo id não duplica");
        assert_eq!(tabela.closest(&id, 1)[0].addr.port(), 2222);
    }

    #[test]
    fn alvo_de_refresh_cai_no_balde_certo() {
        let meu = Id20::random();
        let tabela = RoutingTable::new(meu);
        for index in [0, 1, 7, 8, 63, 159] {
            let alvo = random_in_bucket(&meu, index);
            assert_eq!(
                tabela.bucket_for(&alvo),
                index,
                "alvo do balde {index} caiu no balde errado"
            );
        }
    }
}
