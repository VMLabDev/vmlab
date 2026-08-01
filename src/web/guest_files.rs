//! Moving one file between the browser and a guest:
//! `GET`/`POST /api/labs/{lab}/machines/{machine}/files?path=`.
//!
//! This is the console's half of `machine.push_file`/`machine.pull_file`. The
//! browser holds bytes, not a path the daemon can see, so both directions use
//! the wire's inline forms — the request body becomes `push_file`'s `data`,
//! and `pull_file` with no host path answers with base64 that streams back out
//! as the file. Both are bounded by [`INLINE_FILE_LIMIT`]: over it, the caller
//! is told the limit rather than handed a truncated file.
//!
//! The lab directory's own tree is `files.rs`. This endpoint reaches inside a
//! machine instead, which is why it hangs off the machine's path and not the
//! lab's.

use actix_web::{HttpResponse, web};
use base64::Engine as _;
use serde::Deserialize;

use vmlab::proto::{CommandError, INLINE_FILE_LIMIT, LabRequest, over_inline_limit};

use super::api::fail;
use super::state::AppState;

/// What actix will buffer for a push body. A margin above the wire's ceiling,
/// so a body that merely overshoots the ceiling reaches the handler and is
/// told the limit; far beyond that it is a broken client, and actix refuses it
/// as an oversized payload rather than buffering it.
const MAX_PUSH_BODY: usize = INLINE_FILE_LIMIT as usize + (1 << 16);

/// The endpoint, wired: both directions and the body ceiling the push needs,
/// in one place so the route and its limit cannot drift apart.
pub fn service() -> actix_web::Resource {
    web::resource("/api/labs/{lab}/machines/{machine}/files")
        .app_data(web::PayloadConfig::new(MAX_PUSH_BODY))
        .route(web::get().to(pull))
        .route(web::post().to(push))
}

#[derive(Deserialize)]
pub struct GuestFileQuery {
    /// The file's path inside the guest, guest-absolute.
    path: String,
}

/// `POST /api/labs/{lab}/machines/{machine}/files?path=` — the raw request
/// body becomes a file in the guest.
pub async fn push(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    query: web::Query<GuestFileQuery>,
    body: web::Bytes,
) -> HttpResponse {
    let (lab, machine) = path.into_inner();
    if query.path.trim().is_empty() {
        return fail(CommandError::invalid("no guest path given"));
    }
    if body.len() as u64 > INLINE_FILE_LIMIT {
        return fail(over_inline_limit(
            format!("{} bytes", body.len()),
            INLINE_FILE_LIMIT,
        ));
    }
    let req = LabRequest::MachinePushFile {
        machine,
        to: query.path.clone(),
        from: None,
        data: Some(base64::engine::general_purpose::STANDARD.encode(&body)),
        mode: None,
    };
    match state.lab_call(&lab, req).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/machines/{machine}/files?path=` — the guest file,
/// streamed back to the browser as an attachment.
pub async fn pull(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    query: web::Query<GuestFileQuery>,
) -> HttpResponse {
    let (lab, machine) = path.into_inner();
    if query.path.trim().is_empty() {
        return fail(CommandError::invalid("no guest path given"));
    }
    let req = LabRequest::MachinePullFile {
        machine,
        from: query.path.clone(),
        // No host path: the bytes come back inline, which is the only form a
        // browser can receive.
        to: None,
    };
    let reply = match state.lab_call(&lab, req).await {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let Some(data) = reply.get("data").and_then(|d| d.as_str()) else {
        return fail(CommandError::failed(
            "the daemon answered a pull without the file's bytes",
        ));
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(e) => return fail(CommandError::failed(format!("undecodable file bytes: {e}"))),
    };
    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .insert_header(("Cache-Control", "no-store"))
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", attachment_name(&query.path)),
        ))
        .body(bytes)
}

/// The guest path's last segment, as a header-safe filename.
///
/// Guest paths use either separator (a Windows guest answers with `\`), and a
/// name reaching a `Content-Disposition` header decides nothing else — so
/// quotes, backslashes and control characters come out rather than being
/// escaped, and a path with no usable last segment falls back to a fixed name.
fn attachment_name(guest_path: &str) -> String {
    let last = guest_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim();
    let safe: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '\\'))
        .collect();
    if safe.is_empty() {
        "download".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

    fn state() -> web::Data<AppState> {
        web::Data::new(AppState::new(
            super::super::state::AuthConfig {
                enabled: false,
                user: String::new(),
                password_hash: String::new(),
            },
            Some(("lab".into(), std::path::PathBuf::from("/nonexistent"))),
            false,
        ))
    }

    /// A file over the ceiling is refused here, by code, naming the limit —
    /// the daemon never sees it, and nothing is truncated on the way.
    #[actix_web::test]
    async fn an_oversized_push_is_refused_with_the_limit() {
        let app = test::init_service(App::new().app_data(state()).service(service())).await;
        let body = vec![0u8; INLINE_FILE_LIMIT as usize + 1];
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/machines/dc01/files?path=/tmp/big.bin")
                .set_payload(body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"], "invalid_argument");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains(&INLINE_FILE_LIMIT.to_string()),
            "{body}"
        );
    }

    /// Both directions need somewhere in the guest to work on, and an empty
    /// `path=` is the caller's mistake rather than a transfer that fails deep
    /// in the daemon.
    #[actix_web::test]
    async fn an_empty_guest_path_is_the_callers_mistake() {
        let app = test::init_service(App::new().app_data(state()).service(service())).await;
        for req in [
            test::TestRequest::get().uri("/api/labs/lab/machines/dc01/files?path=%20"),
            test::TestRequest::post()
                .uri("/api/labs/lab/machines/dc01/files?path=")
                .set_payload(vec![1u8, 2, 3]),
        ] {
            let resp = test::call_service(&app, req.to_request()).await;
            assert_eq!(resp.status(), 400);
        }
    }

    /// The name the browser saves under comes from the guest path, whichever
    /// separator the guest uses, and cannot break out of the header it sits in.
    #[actix_web::test]
    async fn attachment_names_come_from_the_last_path_segment() {
        assert_eq!(attachment_name("/var/log/syslog"), "syslog");
        assert_eq!(attachment_name(r"C:\Windows\notes.txt"), "notes.txt");
        assert_eq!(attachment_name("/var/log/"), "download");
        assert_eq!(attachment_name("/tmp/od\"d\r\n.txt"), "odd.txt");
    }
}
