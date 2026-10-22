use std::fs::File;

use camino::Utf8PathBuf;

#[derive(Debug)]
pub struct DirHandleResource {
    pub path: Utf8PathBuf,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct FileHandleResource {
    pub path: Utf8PathBuf,
    pub file: File,
}

#[derive(Debug)]
pub struct ProcessResource {
    #[allow(dead_code)]
    pub command: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_pos: usize,
    pub stderr_pos: usize,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// A browser session the guest opened through the WIT `browser` interface.
pub struct BrowserSessionResource {
    pub driver: thirtyfour::WebDriver,
}

/// An element the guest found, with the session it belongs to (whose page each action re-checks).
pub struct BrowserElementResource {
    pub element: thirtyfour::WebElement,
    pub driver: thirtyfour::WebDriver,
}
