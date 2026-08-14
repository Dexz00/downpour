//! Recursos do executável no Windows: o ícone e o bloco de versão.
//!
//! O caminho usual seria escrever um `.rc` e compilá-lo com o `rc.exe` do SDK
//! do Windows, mas ele não vem com o Rust e raramente está no PATH — o build
//! quebraria em máquina limpa. O formato `.res` é uma sequência de registros
//! simples, então ele é montado aqui, e o `link.exe` aceita o arquivo direto.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

/// O Windows usa como ícone do arquivo o `RT_GROUP_ICON` de menor id.
const ID_GRUPO: u16 = 1;
const LINGUA: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=assets/torrra.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // Só MSVC: o `ld` do mingw não aceita `.res` sem passar pelo `windres`.
    let alvo_windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let alvo_msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !alvo_windows || !alvo_msvc {
        return;
    }

    let icone = Path::new("assets/torrra.ico");
    if !icone.exists() {
        println!("cargo:warning=assets/torrra.ico não existe; o executável sai sem ícone");
        return;
    }

    let ico = fs::read(icone).expect("lendo assets/torrra.ico");
    let versao = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let res = montar_res(&ico, &versao);

    let destino = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("torrra.res");
    fs::write(&destino, res).expect("gravando o .res");
    println!("cargo:rustc-link-arg-bins={}", destino.display());
}

// -- montagem do .res -------------------------------------------------------

fn montar_res(ico: &[u8], versao: &str) -> Vec<u8> {
    let imagens = ler_ico(ico);
    let mut res = Vec::new();

    // Todo `.res` começa com um registro vazio, que marca o formato.
    registro(&mut res, 0, 0, 0, 0, &[]);

    // Cada imagem vira um RT_ICON; o RT_GROUP_ICON é o índice que as costura.
    for (i, img) in imagens.iter().enumerate() {
        registro(&mut res, RT_ICON, i as u16 + 1, 0x1010, LINGUA, &img.dados);
    }
    registro(
        &mut res,
        RT_GROUP_ICON,
        ID_GRUPO,
        0x1030,
        LINGUA,
        &grupo_de_icones(&imagens),
    );
    registro(&mut res, RT_VERSION, 1, 0x0030, LINGUA, &version_info(versao));
    res
}

/// Um registro do `.res`: cabeçalho de tamanho fixo (tipo e nome como ordinais)
/// seguido dos dados, tudo alinhado em 4 bytes.
fn registro(res: &mut Vec<u8>, tipo: u16, nome: u16, flags: u16, lingua: u16, dados: &[u8]) {
    res.extend_from_slice(&(dados.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes()); // tamanho deste cabeçalho
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // tipo vem como ordinal…
    res.extend_from_slice(&tipo.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // …e o nome também
    res.extend_from_slice(&nome.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    res.extend_from_slice(&lingua.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(dados);
    alinhar(res);
}

struct Imagem {
    largura: u8,
    altura: u8,
    cores: u8,
    planos: u16,
    bits: u16,
    dados: Vec<u8>,
}

/// Lê o diretório do `.ico`. Largura e altura são bytes, com 0 significando 256.
fn ler_ico(ico: &[u8]) -> Vec<Imagem> {
    assert!(ico.len() > 6, "assets/torrra.ico está truncado");
    let quantas = u16::from_le_bytes([ico[4], ico[5]]) as usize;

    (0..quantas)
        .map(|i| {
            let e = 6 + i * 16;
            let tamanho = u32::from_le_bytes(ico[e + 8..e + 12].try_into().unwrap()) as usize;
            let inicio = u32::from_le_bytes(ico[e + 12..e + 16].try_into().unwrap()) as usize;
            Imagem {
                largura: ico[e],
                altura: ico[e + 1],
                cores: ico[e + 2],
                planos: u16::from_le_bytes([ico[e + 4], ico[e + 5]]),
                bits: u16::from_le_bytes([ico[e + 6], ico[e + 7]]),
                dados: ico[inicio..inicio + tamanho].to_vec(),
            }
        })
        .collect()
}

/// O GRPICONDIR: o mesmo diretório do `.ico`, mas cada entrada aponta para o id
/// de um recurso em vez de um deslocamento no arquivo.
fn grupo_de_icones(imagens: &[Imagem]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0u16.to_le_bytes()); // reservado
    v.extend_from_slice(&1u16.to_le_bytes()); // 1 = ícone
    v.extend_from_slice(&(imagens.len() as u16).to_le_bytes());
    for (i, img) in imagens.iter().enumerate() {
        v.push(img.largura);
        v.push(img.altura);
        v.push(img.cores);
        v.push(0); // reservado
        v.extend_from_slice(&img.planos.to_le_bytes());
        v.extend_from_slice(&img.bits.to_le_bytes());
        v.extend_from_slice(&(img.dados.len() as u32).to_le_bytes());
        v.extend_from_slice(&(i as u16 + 1).to_le_bytes()); // id do RT_ICON
    }
    v
}

// -- VERSIONINFO ------------------------------------------------------------

/// O bloco que o Explorer mostra em Propriedades e que dá nome ao processo no
/// Gerenciador de Tarefas. É uma árvore de nós de formato uniforme.
fn version_info(versao: &str) -> Vec<u8> {
    let n: Vec<u16> = versao
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse().ok())
        .chain([0, 0, 0, 0])
        .take(4)
        .collect();
    let (maior, menor, correcao, build) = (n[0] as u32, n[1] as u32, n[2] as u32, n[3] as u32);

    let mut fixo = Vec::new();
    fixo.extend_from_slice(&0xFEEF04BDu32.to_le_bytes()); // assinatura
    fixo.extend_from_slice(&0x00010000u32.to_le_bytes()); // versão da struct
    fixo.extend_from_slice(&(maior << 16 | menor).to_le_bytes()); // arquivo
    fixo.extend_from_slice(&(correcao << 16 | build).to_le_bytes());
    fixo.extend_from_slice(&(maior << 16 | menor).to_le_bytes()); // produto
    fixo.extend_from_slice(&(correcao << 16 | build).to_le_bytes());
    fixo.extend_from_slice(&0x3Fu32.to_le_bytes()); // FileFlagsMask
    fixo.extend_from_slice(&0u32.to_le_bytes()); // FileFlags
    fixo.extend_from_slice(&0x00040004u32.to_le_bytes()); // VOS_NT_WINDOWS32
    fixo.extend_from_slice(&1u32.to_le_bytes()); // VFT_APP
    fixo.extend_from_slice(&0u32.to_le_bytes()); // subtipo
    fixo.extend_from_slice(&0u32.to_le_bytes()); // data (alta)
    fixo.extend_from_slice(&0u32.to_le_bytes()); // data (baixa)

    let versao_curta = format!("{maior}.{menor}.{correcao}");
    let versao_longa = format!("{maior}.{menor}.{correcao}.{build}");
    let textos = [
        ("CompanyName", "torrra"),
        ("FileDescription", "Torrent 20X — cliente BitTorrent"),
        ("FileVersion", versao_longa.as_str()),
        ("InternalName", "torrra"),
        ("OriginalFilename", "torrra.exe"),
        ("ProductName", "Torrent 20X"),
        ("ProductVersion", versao_curta.as_str()),
    ];

    let tabela = no(
        // O nome da tabela é a língua seguida da página de código: 0409 04B0.
        &format!("{LINGUA:04X}04B0"),
        1,
        &[],
        0,
        &textos.map(|(k, v)| texto(k, v)),
    );
    let traducao = {
        let mut v = Vec::new();
        v.extend_from_slice(&LINGUA.to_le_bytes());
        v.extend_from_slice(&0x04B0u16.to_le_bytes());
        no("Translation", 0, &v, 4, &[])
    };

    no(
        "VS_VERSION_INFO",
        0,
        &fixo,
        fixo.len() as u16,
        &[
            no("StringFileInfo", 1, &[], 0, &[tabela]),
            no("VarFileInfo", 1, &[], 0, &[traducao]),
        ],
    )
}

/// Um par chave/valor de texto. Para nós de texto o comprimento do valor é
/// contado em caracteres, não em bytes — é o erro clássico deste formato.
fn texto(chave: &str, valor: &str) -> Vec<u8> {
    let utf16 = utf16z(valor);
    no(chave, 1, &utf16, (utf16.len() / 2) as u16, &[])
}

/// Nó do VERSIONINFO: tamanho total, tamanho do valor, tipo, chave em UTF-16,
/// o valor e os filhos — cada parte alinhada em 4 bytes.
fn no(chave: &str, tipo: u16, valor: &[u8], comprimento_valor: u16, filhos: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0u16.to_le_bytes()); // reservado para o tamanho
    v.extend_from_slice(&comprimento_valor.to_le_bytes());
    v.extend_from_slice(&tipo.to_le_bytes());
    v.extend_from_slice(&utf16z(chave));
    alinhar(&mut v);
    v.extend_from_slice(valor);
    alinhar(&mut v);
    for filho in filhos {
        v.extend_from_slice(filho);
        alinhar(&mut v);
    }
    let total = u16::try_from(v.len()).expect("nó do VERSIONINFO maior que 64 KiB");
    v[..2].copy_from_slice(&total.to_le_bytes());
    v
}

/// UTF-16 little-endian terminado em nulo.
fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn alinhar(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}
