//! O pouco que muda quando o programa é aberto com um duplo clique, ou com um
//! arquivo arrastado para cima dele, em vez de pelo terminal.
//!
//! Nesse caso o console é criado para nós e fecha junto com o processo: nada de
//! shell para herdar codificação, nem para deixar a mensagem de erro na tela, e
//! o diretório atual não é o que o usuário tem em mente.

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    const UTF8: u32 = 65_001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCP(pagina: u32) -> i32;
        fn SetConsoleOutputCP(pagina: u32) -> i32;
        fn SetConsoleTitleW(titulo: *const u16) -> i32;
        fn GetConsoleProcessList(lista: *mut u32, quantos: u32) -> u32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: isize, saida: *mut *mut u16)
        -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(bloco: *mut core::ffi::c_void);
    }

    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }

    /// `FOLDERID_Downloads`. Vale mais que `%USERPROFILE%\Downloads`: a pasta
    /// pode ter sido movida para outro disco, e aí o palpite erra.
    const DOWNLOADS: Guid = Guid {
        a: 0x374D_E290,
        b: 0x123F,
        c: 0x4565,
        d: [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B],
    };

    /// Põe o console em UTF-8 e dá nome à janela. Sem isto o console nasce na
    /// página de código antiga e os acentos e os blocos da interface saem
    /// trocados.
    pub fn preparar_console() {
        let titulo: Vec<u16> = "Torrent 20X".encode_utf16().chain([0]).collect();
        unsafe {
            SetConsoleOutputCP(UTF8);
            SetConsoleCP(UTF8);
            SetConsoleTitleW(titulo.as_ptr());
        }
    }

    /// Verdadeiro quando somos o único processo no console — ou seja, ninguém
    /// nos chamou de um shell: o console foi criado para nós e some quando
    /// sairmos.
    pub fn aberto_pelo_gerenciador() -> bool {
        let mut lista = [0u32; 4];
        unsafe { GetConsoleProcessList(lista.as_mut_ptr(), lista.len() as u32) == 1 }
    }

    pub fn pasta_downloads() -> Option<PathBuf> {
        let mut bruto: *mut u16 = std::ptr::null_mut();
        let resultado = unsafe { SHGetKnownFolderPath(&DOWNLOADS, 0, 0, &mut bruto) };
        if resultado < 0 || bruto.is_null() {
            return None;
        }
        // A string vem do alocador do COM e é nossa para liberar.
        let mut fim = 0;
        while unsafe { *bruto.add(fim) } != 0 {
            fim += 1;
        }
        let caminho = OsString::from_wide(unsafe { std::slice::from_raw_parts(bruto, fim) });
        unsafe { CoTaskMemFree(bruto.cast()) };
        Some(PathBuf::from(caminho))
    }

    /// Segura a janela para o usuário ler o que aconteceu. Não faz nada quando
    /// há um shell por trás — lá a mensagem continua na tela sozinha.
    pub fn segurar_janela() {
        if !aberto_pelo_gerenciador() {
            return;
        }
        eprintln!("\nAperte Enter para fechar.");
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut String::new());
    }
}

#[cfg(not(windows))]
mod outros {
    use std::path::PathBuf;

    pub fn preparar_console() {}

    /// Fora do Windows não há esse modo de uso: um terminal aberto por um
    /// gerenciador de arquivos continua aberto depois que o programa sai.
    pub fn aberto_pelo_gerenciador() -> bool {
        false
    }

    pub fn pasta_downloads() -> Option<PathBuf> {
        None
    }

    pub fn segurar_janela() {}
}

#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub use outros::*;
