//! The small amount that changes when the program is opened with a double
//! click, or with a file dragged onto it, instead of from a terminal.
//!
//! In that case the console is created for us and closes together with the
//! process: there is no shell to inherit an encoding from, nor to leave the
//! error message on screen, and the current directory is not what the user
//! has in mind.

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    const UTF8: u32 = 65_001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCP(page: u32) -> i32;
        fn SetConsoleOutputCP(page: u32) -> i32;
        fn SetConsoleTitleW(title: *const u16) -> i32;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(
            id: *const Guid,
            flags: u32,
            token: isize,
            out: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(ptr: *mut core::ffi::c_void);
    }

    #[repr(C)]
    struct Guid {
        a: u32,
        b: u16,
        c: u16,
        d: [u8; 8],
    }

    /// `FOLDERID_Downloads`. Worth more than `%USERPROFILE%\Downloads`: the
    /// folder may have been moved to another drive, and then the guess misses.
    const DOWNLOADS: Guid = Guid {
        a: 0x374D_E290,
        b: 0x123F,
        c: 0x4565,
        d: [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B],
    };

    /// Puts the console in UTF-8 and names the window. Without this the
    /// console starts in the legacy code page and the interface's accents
    /// and blocks come out scrambled.
    pub fn prepare_console() {
        let title: Vec<u16> = "Downpour".encode_utf16().chain([0]).collect();
        unsafe {
            SetConsoleOutputCP(UTF8);
            SetConsoleCP(UTF8);
            SetConsoleTitleW(title.as_ptr());
        }
    }

    /// True when we are the only process on the console — i.e. nobody called
    /// us from a shell: the console was created for us and disappears when
    /// we exit.
    pub fn opened_by_file_manager() -> bool {
        let mut list = [0u32; 4];
        unsafe { GetConsoleProcessList(list.as_mut_ptr(), list.len() as u32) == 1 }
    }

    pub fn downloads_folder() -> Option<PathBuf> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let result = unsafe { SHGetKnownFolderPath(&DOWNLOADS, 0, 0, &mut raw) };
        if result < 0 || raw.is_null() {
            return None;
        }
        // The string comes from the COM allocator and is ours to free.
        let mut end = 0;
        while unsafe { *raw.add(end) } != 0 {
            end += 1;
        }
        let path = OsString::from_wide(unsafe { std::slice::from_raw_parts(raw, end) });
        unsafe { CoTaskMemFree(raw.cast()) };
        Some(PathBuf::from(path))
    }

    /// Holds the window open so the user can read what happened. Does
    /// nothing when there is a shell behind us — there the message stays
    /// on screen by itself.
    pub fn hold_window() {
        if !opened_by_file_manager() {
            return;
        }
        eprintln!("\nPress Enter to close.");
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut String::new());
    }
}

#[cfg(not(windows))]
mod other {
    use std::path::PathBuf;

    pub fn prepare_console() {}

    /// Outside Windows there is no such usage mode: a terminal opened by a
    /// file manager stays open after the program exits.
    pub fn opened_by_file_manager() -> bool {
        false
    }

    pub fn downloads_folder() -> Option<PathBuf> {
        None
    }

    pub fn hold_window() {}
}

#[cfg(windows)]
pub use windows::*;

#[cfg(not(windows))]
pub use other::*;
