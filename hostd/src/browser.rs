//! The browser, shared by both executors (the planner's JSON actions and the WIT `browser`
//! interface): sessions are opened, pages visited and elements used only as the host's rules allow.
//! A visit goes only to `allowed_hosts` ([`sandbox::check_navigation`]). Every other action first
//! checks that the session's page is still on them, because a page can navigate itself. Each call
//! blocks on the tokio runtime, so it runs on the plan-act loop's blocking thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use camino::Utf8Path;
use thirtyfour::prelude::*;
use tokio::runtime::Handle;

use crate::config::BrowserSettings;
use crate::sandbox;

/// The longest a visit waits for a page to settle.
const MAX_SETTLE: Duration = Duration::from_secs(30);

/// The most browser sessions a run may hold open at once, across both executors.
pub const MAX_SESSIONS: usize = 4;

/// The longest the host waits for a session to quit.
pub const QUIT_WAIT: Duration = Duration::from_secs(5);

/// A browser call the host's rules refused, as opposed to one that failed.
#[derive(Debug)]
pub enum Refusal {
    /// Against a rule: the allowed hosts, a read-only workspace.
    Denied(String),
    /// Past a cap.
    Limit(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Denied(why) | Refusal::Limit(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Refusal {}

/// Quit `driver`, waiting at most `wait`. A quit that fails or does not finish in time is given
/// up, and the handle told not to try again when dropped (which would wait up to the WebDriver
/// client's own timeout).
fn quit_within(tokio: &Handle, driver: WebDriver, wait: Duration) {
    let quitting = driver.clone();
    let quit = tokio.block_on(async move { tokio::time::timeout(wait, quitting.quit()).await });
    if !matches!(quit, Ok(Ok(()))) {
        let _ = driver.leak();
    }
}

/// The run's open sessions, shared by both executors: they count against [`MAX_SESSIONS`], and when
/// the watchdog ends a run it quits whatever is still open.
#[derive(Clone, Default)]
pub struct Sessions(Arc<Mutex<Vec<WebDriver>>>);

impl Sessions {
    /// The open sessions. Nothing panics while holding the lock, so a poisoned one is still sound.
    fn open(&self) -> std::sync::MutexGuard<'_, Vec<WebDriver>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn open_count(&self) -> usize {
        self.open().len()
    }

    fn add(&self, driver: &WebDriver) {
        self.open().push(driver.clone());
    }

    fn remove(&self, driver: &WebDriver) {
        self.open()
            .retain(|d| !Arc::ptr_eq(&d.handle, &driver.handle));
    }

    /// Quit every open session, each within [`QUIT_WAIT`]. A session still being opened is not
    /// yet here.
    pub fn quit_all(&self, tokio: &Handle) {
        let open = std::mem::take(&mut *self.open());
        for driver in open {
            quit_within(tokio, driver, QUIT_WAIT);
        }
    }
}

/// How an element is found.
pub enum Select {
    Css(String),
    XPath(String),
    /// An element whose own text is exactly this (whitespace normalised).
    Text(String),
}

impl Select {
    fn by(&self) -> Result<By> {
        Ok(match self {
            Select::Css(css) => By::Css(css.clone()),
            Select::XPath(xpath) => By::XPath(xpath.clone()),
            Select::Text(text) => {
                let literal = serde_json::to_string(text)?;
                By::XPath(format!("//*[normalize-space(text()) = {literal}]"))
            }
        })
    }
}

/// What a page shows.
pub struct Page {
    pub url: String,
    pub title: Option<String>,
    pub html: Option<String>,
}

/// The browser as configured for one run.
pub struct Browser<'a> {
    pub tokio: &'a Handle,
    pub settings: &'a BrowserSettings,
    pub workspace: &'a Utf8Path,
    /// The workspace is read-only, so nothing may download into it.
    pub read_only: bool,
    pub sessions: &'a Sessions,
    /// The run's wall-clock time left: no wait runs past it.
    pub remaining: Duration,
}

impl Browser<'_> {
    fn block_on<T>(&self, work: impl Future<Output = WebDriverResult<T>>) -> Result<T> {
        Ok(self.tokio.block_on(work)?)
    }

    /// Open a session. Chrome keeps its sandbox unless `chrome_no_sandbox` is set, and allowed
    /// downloads land in the workspace (refused when it is read-only). At most [`MAX_SESSIONS`] may
    /// be open at once.
    pub fn open(&self, headless: bool, allow_downloads: bool) -> Result<WebDriver> {
        if allow_downloads && self.read_only {
            return Err(Refusal::Denied(
                "downloads are refused: the workspace is read-only".to_string(),
            )
            .into());
        }
        if self.sessions.open_count() >= MAX_SESSIONS {
            return Err(Refusal::Limit(format!(
                "at most {MAX_SESSIONS} browser sessions may be open at once"
            ))
            .into());
        }
        let url = self.settings.webdriver_url.clone();
        let no_sandbox = self.settings.chrome_no_sandbox;
        let download_dir = self.workspace.to_string();
        self.block_on(async move {
            let mut caps = DesiredCapabilities::chrome();
            if headless {
                caps.add_arg("--headless=new")?;
                caps.add_arg("--disable-gpu")?;
            }
            caps.add_arg("--disable-dev-shm-usage")?;
            if no_sandbox {
                caps.add_arg("--no-sandbox")?;
            }
            if allow_downloads {
                // Into the workspace, not the user's Downloads folder.
                caps.add_experimental_option(
                    "prefs",
                    serde_json::json!({
                        "download.prompt_for_download": false,
                        "download.default_directory": download_dir,
                    }),
                )?;
            }
            WebDriver::new(&url, caps).await
        })
        .inspect(|driver| self.sessions.add(driver))
    }

    /// End a session, waiting at most [`QUIT_WAIT`].
    pub fn quit(&self, driver: WebDriver) {
        self.sessions.remove(&driver);
        quit_within(self.tokio, driver, QUIT_WAIT);
    }

    fn current_url(&self, driver: &WebDriver) -> Result<String> {
        let driver = driver.clone();
        self.block_on(async move { driver.current_url().await.map(|u| u.to_string()) })
    }

    fn leave_for_blank(&self, driver: &WebDriver) -> Result<()> {
        let driver = driver.clone();
        self.block_on(async move { driver.goto("about:blank").await })
    }

    /// The session's page must still be on the allowed hosts (or blank). One that wandered off is
    /// left for `about:blank` and the action refused.
    pub fn check_page(&self, driver: &WebDriver) -> Result<()> {
        let current = self.current_url(driver)?;
        if sandbox::is_blank_page(&current) {
            return Ok(());
        }
        if let Err(why) = sandbox::check_navigation(&current, &self.settings.allowed_hosts) {
            self.leave_for_blank(driver)?;
            return Err(Refusal::Denied(format!(
                "the page left the allowed hosts ({why}); left for about:blank"
            ))
            .into());
        }
        Ok(())
    }

    /// Visit `url`, waiting up to `settle` for the page; returns where the browser ended up. The URL
    /// as parsed is what the browser gets, and a page that redirects off the allowed hosts is left
    /// for `about:blank`.
    pub fn goto(&self, driver: &WebDriver, url: &str, settle: Duration) -> Result<String> {
        let url = sandbox::check_navigation(url, &self.settings.allowed_hosts)
            .map_err(Refusal::Denied)?;
        let settle = settle.min(MAX_SETTLE).min(self.remaining);
        {
            let driver = driver.clone();
            self.block_on(async move {
                driver.goto(&url).await?;
                tokio::time::sleep(settle).await;
                Ok(())
            })?;
        }
        let landed = self.current_url(driver)?;
        if let Err(why) = sandbox::check_navigation(&landed, &self.settings.allowed_hosts) {
            self.leave_for_blank(driver)?;
            return Err(Refusal::Denied(format!(
                "the page redirected off the allowed hosts ({why}); left for about:blank"
            ))
            .into());
        }
        Ok(landed)
    }

    /// What the page shows: its URL, its title and, if asked, its HTML.
    pub fn describe(&self, driver: &WebDriver, include_html: bool) -> Result<Page> {
        self.check_page(driver)?;
        let driver = driver.clone();
        self.block_on(async move {
            let url = driver.current_url().await?.to_string();
            let title = driver.title().await.ok();
            let html = if include_html {
                Some(driver.source().await?)
            } else {
                None
            };
            Ok(Page { url, title, html })
        })
    }

    /// The first element `select` matches, waiting up to `wait` for one.
    pub fn find(&self, driver: &WebDriver, select: &Select, wait: Duration) -> Result<WebElement> {
        self.check_page(driver)?;
        let by = select.by()?;
        let driver = driver.clone();
        let wait = wait.min(MAX_SETTLE).min(self.remaining);
        self.block_on(async move {
            driver
                .query(by)
                .wait(wait, Duration::from_millis(200))
                .first()
                .await
        })
    }

    /// Every element `select` matches now.
    pub fn find_all(&self, driver: &WebDriver, select: &Select) -> Result<Vec<WebElement>> {
        self.check_page(driver)?;
        let by = select.by()?;
        let driver = driver.clone();
        self.block_on(async move { driver.find_all(by).await })
    }

    pub fn click(&self, driver: &WebDriver, element: &WebElement) -> Result<()> {
        self.check_page(driver)?;
        let element = element.clone();
        self.block_on(async move { element.click().await })
    }

    pub fn type_text(
        &self,
        driver: &WebDriver,
        element: &WebElement,
        text: &str,
        submit: bool,
    ) -> Result<()> {
        self.check_page(driver)?;
        let (element, text) = (element.clone(), text.to_string());
        self.block_on(async move {
            element.send_keys(text).await?;
            if submit {
                element.send_keys(Key::Enter).await?;
            }
            Ok(())
        })
    }

    pub fn clear(&self, driver: &WebDriver, element: &WebElement) -> Result<()> {
        self.check_page(driver)?;
        let element = element.clone();
        self.block_on(async move { element.clear().await })
    }

    pub fn attribute(
        &self,
        driver: &WebDriver,
        element: &WebElement,
        name: &str,
    ) -> Result<Option<String>> {
        self.check_page(driver)?;
        let (element, name) = (element.clone(), name.to_string());
        self.block_on(async move { element.attr(&name).await })
    }

    pub fn inner_text(&self, driver: &WebDriver, element: &WebElement) -> Result<String> {
        self.check_page(driver)?;
        let element = element.clone();
        self.block_on(async move { element.text().await })
    }

    pub fn outer_html(&self, driver: &WebDriver, element: &WebElement) -> Result<String> {
        self.check_page(driver)?;
        let element = element.clone();
        self.block_on(async move { element.outer_html().await })
    }

    /// A PNG of the page.
    pub fn screenshot(&self, driver: &WebDriver) -> Result<Vec<u8>> {
        self.check_page(driver)?;
        let driver = driver.clone();
        self.block_on(async move { driver.screenshot_as_png().await })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A WebDriver that opens sessions and answers each request to end one after `delay`, with
    /// HTTP `status`; returns its URL and how many ends it was asked for.
    fn webdriver(delay: Duration, status: u16) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let quits = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&quits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let count = Arc::clone(&count);
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut length = 0;
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                                return;
                            }
                            if header == "\r\n" {
                                break;
                            }
                            if let Some(v) =
                                header.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = v.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        let _ = reader.read_exact(&mut body);
                        let (code, reply) = if line.starts_with("DELETE") {
                            count.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(delay);
                            (status, "{\"value\":null}".to_string())
                        } else if line.starts_with("POST /session ") {
                            (
                                200,
                                "{\"value\":{\"sessionId\":\"s1\",\"capabilities\":{}}}"
                                    .to_string(),
                            )
                        } else {
                            (200, "{\"value\":null}".to_string())
                        };
                        let _ = write!(
                            writer,
                            "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                            reply.len()
                        );
                    }
                });
            }
        });
        (url, quits)
    }

    fn session(tokio: &tokio::runtime::Runtime, url: &str) -> WebDriver {
        let url = url.to_string();
        tokio
            .block_on(async move { WebDriver::new(&url, DesiredCapabilities::chrome()).await })
            .expect("session")
    }

    /// A quit that does not finish in time is given up on, and not tried again when the handle is
    /// dropped.
    #[test]
    fn a_quit_that_times_out_is_not_retried() {
        let tokio = tokio::runtime::Runtime::new().expect("runtime");
        let (url, quits) = webdriver(Duration::from_secs(3), 200);
        let driver = session(&tokio, &url);
        let started = std::time::Instant::now();
        quit_within(tokio.handle(), driver, Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(quits.load(Ordering::SeqCst), 1);
    }

    /// A quit that fails is not tried again when the handle is dropped either.
    #[test]
    fn a_failed_quit_is_not_retried() {
        let tokio = tokio::runtime::Runtime::new().expect("runtime");
        let (url, quits) = webdriver(Duration::ZERO, 500);
        let driver = session(&tokio, &url);
        quit_within(tokio.handle(), driver, Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(quits.load(Ordering::SeqCst), 1);
    }
}
