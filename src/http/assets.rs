//! Web assets compiled into the binary (no files needed at runtime).
//!
//! For frontend development, `HWSTATS_WEB_DIR=<repo>/web` serves the same
//! (known) files from disk instead, so a page reload picks up edits.

use std::borrow::Cow;

pub struct Asset {
    pub path: &'static str,
    pub mime: &'static str,
    pub body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        Asset {
            path: $path,
            mime: $mime,
            body: include_bytes!(concat!("../../web/", $path)),
        }
    };
}

pub static ASSETS: &[Asset] = &[
    asset!("index.html", "text/html; charset=utf-8"),
    asset!("style.css", "text/css; charset=utf-8"),
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("cards.js", "text/javascript; charset=utf-8"),
    asset!("charts.js", "text/javascript; charset=utf-8"),
    asset!("format.js", "text/javascript; charset=utf-8"),
    asset!("vbar.js", "text/javascript; charset=utf-8"),
    asset!("procdetail.js", "text/javascript; charset=utf-8"),
    asset!("settings.js", "text/javascript; charset=utf-8"),
    asset!("tween.js", "text/javascript; charset=utf-8"),
    asset!("favicon.svg", "image/svg+xml"),
    asset!("vendor/uPlot.iife.min.js", "text/javascript; charset=utf-8"),
    asset!("vendor/uPlot.min.css", "text/css; charset=utf-8"),
    asset!("vendor/fonts/barlow-400.woff2", "font/woff2"),
    asset!("vendor/fonts/barlow-500.woff2", "font/woff2"),
    asset!("vendor/fonts/barlow-600.woff2", "font/woff2"),
    asset!("vendor/fonts/barlow-condensed-500.woff2", "font/woff2"),
    asset!("vendor/fonts/barlow-condensed-600.woff2", "font/woff2"),
];

/// Look up an asset by its path; returns body and MIME type.
pub fn get(path: &str) -> Option<(Cow<'static, [u8]>, &'static str)> {
    let a = ASSETS.iter().find(|a| a.path == path)?;
    if let Some(dir) = std::env::var_os("HWSTATS_WEB_DIR")
        && let Ok(b) = std::fs::read(std::path::Path::new(&dir).join(a.path))
    {
        return Some((Cow::Owned(b), a.mime));
    }
    Some((Cow::Borrowed(a.body), a.mime))
}
