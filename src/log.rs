//! Diagnóstico opcional.
//!
//! Ligado por `TORRRA_LOG=1` (ou `=peer`, `=dht`, `=tracker`, `=disco` para
//! filtrar). Escreve em stderr, que na TUI pode ser redirecionado para arquivo:
//! `torrra x.torrent 2> log.txt`.

use std::sync::OnceLock;

static FILTRO: OnceLock<Option<String>> = OnceLock::new();

fn filtro() -> &'static Option<String> {
    FILTRO.get_or_init(|| std::env::var("TORRRA_LOG").ok().filter(|v| v != "0"))
}

/// Este assunto deve ser registrado?
pub fn ativo(assunto: &str) -> bool {
    match filtro() {
        Some(f) => f == "1" || f == "all" || f.split(',').any(|parte| parte.trim() == assunto),
        None => false,
    }
}

/// `log!("peer", "handshake falhou: {e}")`
macro_rules! log {
    ($assunto:expr, $($arg:tt)*) => {
        if $crate::log::ativo($assunto) {
            eprintln!("[{}] {}", $assunto, format_args!($($arg)*));
        }
    };
}

pub(crate) use log;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtro_desligado_por_padrao() {
        // O teste roda sem a variável definida no ambiente normal.
        if std::env::var("TORRRA_LOG").is_err() {
            assert!(!ativo("peer"));
        }
    }
}
