//! The browser, shared by both executors (the planner's JSON actions and the WIT `browser`
//! interface): sessions are opened, pages visited and elements used only as the host's rules allow.
//! A visit goes only to `allowed_hosts` ([`sandbox::check_navigation`]). Every other action first
//! checks that the session's page is still on them, because a page can navigate itself. Each call
//! blocks on the tokio runtime, so it runs on the plan-act loop's blocking thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use camino::Utf8Path;
use thirtyfour::prelude::*;
use tokio::runtime::Handle;

use crate::config::BrowserSettings;
use crate::sandbox;

/// The longest a visit waits for a page to settle.
const MAX_SETTLE: Duration = Duration::from_secs(30);

/// The most browser sessions a run may hold open at once, across both executors.
pub const MAX_SESSIONS: usize = 4;

/// The run's open sessions, shared by both executors: they count against [`MAX_SESSIONS`], and when
/// the watchdog ends a run it quits whatever is still open.
#[derive(Clone, Default)]
pub struct Sessions(Arc<Mutex<Vec<WebDriver>>>);

impl Sessions {
    fn open_count(&self) -> usize {
        self.0.lock().map(|s| s.len()).unwrap_or(MAX_SESSIONS)
    }

    fn add(&self, driver: &WebDriver) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.push(driver.clone());
        }
    }

    fn remove(&self, driver: &WebDriver) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.retain(|d| !Arc::ptr_eq(&d.handle, &driver.handle));
        }
    }

    /// Quit every open session, waiting at most `each` for each.
    pub fn quit_all(&self, tokio: &Handle, each: Duration) {
        let open = self
            .0
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default();
        for driver in open {
            let _ = tokio.block_on(tokio::time::timeout(each, driver.quit()));
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
            bail!("downloads are refused: the workspace is read-only");
        }
        if self.sessions.open_count() >= MAX_SESSIONS {
            bail!("at most {MAX_SESSIONS} browser sessions may be open at once");
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

    /// End a session.
    pub fn quit(&self, driver: WebDriver) {
        self.sessions.remove(&driver);
        let _ = self.tokio.block_on(driver.quit());
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
            bail!("the page left the allowed hosts ({why}); left for about:blank");
        }
        Ok(())
    }

    /// Visit `url`, waiting up to `settle` for the page; returns where the browser ended up. The URL
    /// as parsed is what the browser gets, and a page that redirects off the allowed hosts is left
    /// for `about:blank`.
    pub fn goto(&self, driver: &WebDriver, url: &str, settle: Duration) -> Result<String> {
        let url = sandbox::check_navigation(url, &self.settings.allowed_hosts)
            .map_err(anyhow::Error::msg)?;
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
            bail!("the page redirected off the allowed hosts ({why}); left for about:blank");
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
