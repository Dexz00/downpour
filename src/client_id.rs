//! Identificação do software do outro lado, a partir do peer id.
//!
//! Não há campo obrigatório para isso no protocolo: o nome do cliente é uma
//! convenção sobre os 20 bytes do peer id. Saber com quem se está falando ajuda
//! a entender o comportamento do enxame — e é o tipo de detalhe que a interface
//! precisa mostrar.

use crate::id::Id20;

/// Códigos de duas letras do estilo Azureus (`-qB4650-…`).
const CONHECIDOS: &[(&[u8; 2], &str)] = &[
    (b"7T", "aTorrent"),
    (b"AZ", "Vuze"),
    (b"BC", "BitComet"),
    (b"BT", "BitTorrent"),
    (b"DE", "Deluge"),
    (b"FD", "Free Download Manager"),
    (b"KT", "KTorrent"),
    (b"LT", "libtorrent"),
    (b"lt", "libTorrent"),
    (b"PI", "PicoTorrent"),
    (b"qB", "qBittorrent"),
    (b"TR", "Transmission"),
    (b"TX", "Tixati"),
    (b"UM", "µTorrent Mac"),
    (b"UT", "µTorrent"),
    (b"UW", "µTorrent Web"),
    (b"WW", "WebTorrent"),
    (b"XL", "Xunlei"),
    (b"lh", "libtorrent-rasterbar"),
];

/// Nome legível do cliente, ou o peer id em forma imprimível.
pub fn nome(peer_id: &Id20) -> String {
    let b = peer_id.as_bytes();

    // Estilo Azureus: '-' + 2 letras + 4 dígitos de versão + '-'.
    if b[0] == b'-' && b[7] == b'-' {
        let codigo = [b[1], b[2]];
        if let Some((_, nome)) = CONHECIDOS.iter().find(|(c, _)| **c == codigo) {
            return match versao(&b[3..7]) {
                Some(v) => format!("{nome} {v}"),
                None => nome.to_string(),
            };
        }
        if codigo.iter().all(|c| c.is_ascii_alphanumeric()) {
            return format!("{}{}", codigo[0] as char, codigo[1] as char);
        }
    }

    // Estilo Shadow: uma letra seguida de versão em base64-ish.
    if b[0].is_ascii_alphabetic() && b[4..8].iter().all(|c| *c == b'-' || *c == 0) {
        let nome = match b[0] {
            b'A' => "ABC",
            b'T' => "BitTornado",
            b'U' => "UPnP NAT",
            b'S' => "Shadow",
            _ => "",
        };
        if !nome.is_empty() {
            return nome.to_string();
        }
    }

    // BitComet antigo usa um prefixo próprio.
    if b.starts_with(b"exbc") || b.starts_with(b"FUTB") {
        return "BitComet".to_string();
    }
    if b.starts_with(b"OP") {
        return "Opera".to_string();
    }

    imprimivel(b)
}

/// `4650` → `4.6.5.0`, descartando zeros à direita irrelevantes.
fn versao(digitos: &[u8]) -> Option<String> {
    let partes: Vec<u32> = digitos
        .iter()
        .map(|d| match d {
            b'0'..=b'9' => Some((d - b'0') as u32),
            // Versões acima de 9 usam letras.
            b'A'..=b'Z' => Some((d - b'A') as u32 + 10),
            b'a'..=b'z' => Some((d - b'a') as u32 + 36),
            _ => None,
        })
        .collect::<Option<Vec<u32>>>()?;

    let mut fim = partes.len();
    while fim > 2 && partes[fim - 1] == 0 {
        fim -= 1;
    }
    Some(
        partes[..fim]
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// Último recurso: mostra os bytes legíveis do peer id.
fn imprimivel(b: &[u8; 20]) -> String {
    let texto: String = b
        .iter()
        .take(8)
        .map(|c| {
            if c.is_ascii_graphic() {
                *c as char
            } else {
                '·'
            }
        })
        .collect();
    if texto.chars().all(|c| c == '·') {
        "desconhecido".to_string()
    } else {
        texto
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_id(prefixo: &[u8]) -> Id20 {
        let mut b = [b'x'; 20];
        b[..prefixo.len()].copy_from_slice(prefixo);
        Id20(b)
    }

    #[test]
    fn reconhece_clientes_comuns() {
        assert_eq!(nome(&peer_id(b"-qB4650-")), "qBittorrent 4.6.5");
        assert_eq!(nome(&peer_id(b"-TR3000-")), "Transmission 3.0");
        assert_eq!(nome(&peer_id(b"-lt0D60-")), "libTorrent 0.13.6");
        assert_eq!(nome(&peer_id(b"-UT3550-")), "µTorrent 3.5.5");
        assert_eq!(nome(&peer_id(b"-DE2100-")), "Deluge 2.1");
    }

    #[test]
    fn lida_com_clientes_desconhecidos() {
        // Código de duas letras não catalogado ainda aparece.
        assert_eq!(nome(&peer_id(b"-ZZ1000-")), "ZZ");
        assert_eq!(nome(&peer_id(b"exbc")), "BitComet");

        // Sem convenção nenhuma, mostra os bytes legíveis.
        let bruto = nome(&peer_id(b"abc"));
        assert!(bruto.starts_with("abc"), "veio `{bruto}`");

        let zerado = nome(&Id20([0; 20]));
        assert_eq!(zerado, "desconhecido");
    }

    #[test]
    fn versao_descarta_zeros_a_direita() {
        assert_eq!(versao(b"4650").unwrap(), "4.6.5");
        assert_eq!(versao(b"3000").unwrap(), "3.0");
        assert_eq!(versao(b"1234").unwrap(), "1.2.3.4");
        // Letras representam números acima de 9.
        assert_eq!(versao(b"0D60").unwrap(), "0.13.6");
        assert!(versao(b"!!!!").is_none());
    }
}
