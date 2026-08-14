//! Medição de taxa de transferência.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Resolução dos baldes da janela deslizante.
const BALDE: Duration = Duration::from_millis(500);
/// Quantos baldes a janela guarda (8 segundos).
const BALDES: usize = 16;

/// Taxa em bytes por segundo, medida numa janela deslizante.
///
/// Uma média sobre o download inteiro esconde exatamente o que interessa: se a
/// velocidade caiu *agora*. A janela curta responde rápido sem tremer a cada
/// bloco recebido.
pub struct RateMeter {
    baldes: [u64; BALDES],
    atual: usize,
    virada: Instant,
    total: u64,
}

impl Default for RateMeter {
    fn default() -> Self {
        RateMeter::new()
    }
}

impl RateMeter {
    pub fn new() -> RateMeter {
        RateMeter {
            baldes: [0; BALDES],
            atual: 0,
            virada: Instant::now(),
            total: 0,
        }
    }

    pub fn add(&mut self, bytes: u64) {
        self.rotate();
        self.baldes[self.atual] += bytes;
        self.total += bytes;
    }

    /// Bytes por segundo na janela recente.
    pub fn rate(&mut self) -> f64 {
        self.rotate();
        let soma: u64 = self.baldes.iter().sum();
        soma as f64 / (BALDES as f64 * BALDE.as_secs_f64())
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    fn rotate(&mut self) {
        let decorrido = self.virada.elapsed();
        if decorrido < BALDE {
            return;
        }
        let passos = (decorrido.as_nanos() / BALDE.as_nanos()) as usize;
        if passos >= BALDES {
            self.baldes = [0; BALDES];
            self.atual = 0;
        } else {
            for _ in 0..passos {
                self.atual = (self.atual + 1) % BALDES;
                self.baldes[self.atual] = 0;
            }
        }
        self.virada += BALDE * passos as u32;
    }
}

/// Série temporal curta, para desenhar o gráfico de velocidade.
pub struct History {
    amostras: VecDeque<f64>,
    capacidade: usize,
}

impl History {
    pub fn new(capacidade: usize) -> History {
        History {
            amostras: VecDeque::with_capacity(capacidade),
            capacidade,
        }
    }

    pub fn push(&mut self, valor: f64) {
        if self.amostras.len() == self.capacidade {
            self.amostras.pop_front();
        }
        self.amostras.push_back(valor);
    }

    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.amostras.iter().copied()
    }

}

/// Formata bytes em unidade legível: `1.4 MiB`.
pub fn bytes(n: u64) -> String {
    const UNIDADES: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut valor = n as f64;
    let mut unidade = 0;
    while valor >= 1024.0 && unidade + 1 < UNIDADES.len() {
        valor /= 1024.0;
        unidade += 1;
    }
    if unidade == 0 {
        format!("{n} B")
    } else if valor < 10.0 {
        format!("{valor:.2} {}", UNIDADES[unidade])
    } else {
        format!("{valor:.1} {}", UNIDADES[unidade])
    }
}

/// Formata uma taxa: `2.3 MiB/s`.
pub fn rate(bytes_por_segundo: f64) -> String {
    if bytes_por_segundo < 1.0 {
        return "—".to_string();
    }
    format!("{}/s", bytes(bytes_por_segundo as u64))
}

/// Formata uma duração como tempo restante: `2h 14m`.
pub fn eta(segundos: Option<u64>) -> String {
    let Some(s) = segundos else {
        return "∞".to_string();
    };
    match s {
        0 => "agora".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s if s < 86400 * 99 => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
        _ => "∞".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxa_considera_a_janela_inteira() {
        let mut m = RateMeter::new();
        m.add(8 * 1024);
        // 8 KiB numa janela de 8 segundos = 1 KiB/s.
        assert!((m.rate() - 1024.0).abs() < 1.0, "veio {}", m.rate());
        assert_eq!(m.total(), 8 * 1024);

        m.add(8 * 1024);
        assert!((m.rate() - 2048.0).abs() < 1.0);
        assert_eq!(m.total(), 16 * 1024);
    }

    #[test]
    fn janela_esvazia_com_o_tempo() {
        let mut m = RateMeter::new();
        m.add(1000);
        assert!(m.rate() > 0.0);
        // Empurra a virada para o passado além da janela inteira.
        m.virada = Instant::now() - BALDE * (BALDES as u32 + 2);
        assert_eq!(m.rate(), 0.0, "dados antigos saem da janela");
        assert_eq!(m.total(), 1000, "o total acumulado permanece");
    }

    #[test]
    fn historico_respeita_a_capacidade() {
        let mut h = History::new(3);
        for v in [1.0, 2.0, 3.0, 4.0] {
            h.push(v);
        }
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn formata_tamanhos_e_tempos() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(20 * 1024), "20.0 KiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");

        assert_eq!(rate(0.5), "—");
        assert_eq!(rate(2.0 * 1024.0 * 1024.0), "2.00 MiB/s");

        assert_eq!(eta(None), "∞");
        assert_eq!(eta(Some(0)), "agora");
        assert_eq!(eta(Some(45)), "45s");
        assert_eq!(eta(Some(90)), "1m 30s");
        assert_eq!(eta(Some(3700)), "1h 1m");
        assert_eq!(eta(Some(90000)), "1d 1h");
    }
}
