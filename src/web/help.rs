//! `/help` — the embedded documentation book: the vmlab wskill rendered to
//! static HTML by `wcl wdoc` (`just help-build` → `docs/help/`, baked in via
//! rust-embed like the SPA). The render is optional at compile time: without
//! it the help root serves a pointer to the hosted docs instead, so plain
//! `cargo build` still works without the `wcl` CLI installed.

use actix_web::{HttpRequest, HttpResponse};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "docs/help"]
struct HelpAssets;

/// Shown when the book wasn't rendered into this build.
const FALLBACK: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>vmlab help</title></head>
<body style="font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; line-height: 1.6">
<h1>Help isn't bundled into this build</h1>
<p>The vmlab reference book is available online at
<a href="https://vmlab.io/wskills/vmlab/">vmlab.io/wskills/vmlab</a>.</p>
<p>To bundle it, render the book and rebuild the server:
<code>just help-build web-build</code> (needs the <code>wcl</code> CLI).</p>
</body></html>"#;

/// `/help` → `/help/`, so the book's relative links resolve.
pub async fn root() -> HttpResponse {
    HttpResponse::Found()
        .insert_header(("Location", "/help/"))
        .finish()
}

/// `/help/{path}` — serve one page/asset of the rendered book.
pub async fn page(req: HttpRequest) -> HttpResponse {
    let path = req.match_info().query("path");
    let path = if path.is_empty() { "index.html" } else { path };
    match HelpAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            HttpResponse::Ok()
                .content_type(mime.as_ref())
                .body(content.data.into_owned())
        }
        None if path == "index.html" => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(FALLBACK),
        None => HttpResponse::NotFound().finish(),
    }
}
