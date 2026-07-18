//! Reverse proxy for guest-served HTTP UIs declared as `web {}` blocks,
//! modeled on aciddog's `/preview` proxy. Pages open in same-origin iframes
//! under `/web/{lab}/{kind}/{machine}/{page}/…`; the handler forwards to a
//! loopback forward the lab daemon opens into the guest (`web.forward`),
//! strips frame-blocking headers, and rewrites HTML/CSS URLs and redirects
//! so everything stays under the page prefix.
//!
//! Auth (two independent layers):
//! - **Console auth**: iframe subresources can't carry a Bearer header, so
//!   `/web/*` (outside the `/api/` gate) is guarded by the path-scoped
//!   `vmlab_web` cookie minted by `POST /api/web/session`, validated here.
//! - **Upstream auth**: the guest app's own login is performed by the proxy
//!   from credentials in the `web { auth {} }` block (basic / bearer /
//!   header / NTLM / form) so the user never sees it.
//!
//! Deliberately out of scope (v1, like aciddog): WebSockets/SSE, and complex
//! SPAs that build absolute URLs in JS. Fine for admin UIs; not for bulk
//! downloads (the vTCP fabric is modest).

use std::sync::Arc;
use std::time::{Duration, Instant};

use actix_web::http::header;
use actix_web::{HttpRequest, HttpResponse, web};
use base64::Engine;
use regex::Regex;
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::state::{AppState, valid_name};

const WEB_COOKIE: &str = "vmlab_web";
const MAX_BODY: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum gap between form re-login attempts, so a persistently-failing
/// login can't hammer the guest on every subresource.
const RELOGIN_COOLDOWN: Duration = Duration::from_secs(5);

// ---- upstream auth spec (deserialized from the web.forward reply) -----------

/// Mirror of `config::model::WebAuth`, tagged by `method`. Comes over the
/// daemon socket only — never serialized back to the browser.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum WebAuthSpec {
    Basic {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
    Header {
        name: String,
        value: String,
    },
    Ntlm {
        username: String,
        password: String,
        #[serde(default)]
        domain: Option<String>,
    },
    Form {
        username: String,
        password: String,
        login_path: String,
        login_method: String,
        login_body: String,
        login_content_type: String,
        #[serde(default)]
        fail_redirect: Option<String>,
    },
}

// ---- resolved target --------------------------------------------------------

/// A resolved proxy target: the loopback forward address plus a per-page
/// reqwest client and upstream-auth session. One per (lab, kind, machine,
/// page); the persistent client is what lets NTLM keep its authenticated
/// connection and form auth keep its cookie jar.
pub struct WebTarget {
    pub addr: std::net::SocketAddr,
    pub guest_ip: String,
    pub port: u16,
    pub auth: Option<WebAuthSpec>,
    pub client: reqwest::Client,
    /// The machine name the client's connection is pinned to (`resolve`
    /// key); also the Host the guest sees. Used to build upstream URLs.
    host: String,
    session: Mutex<AuthSession>,
}

impl std::fmt::Debug for WebTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render credentials (a stray {:?} in a log/error must not leak).
        f.debug_struct("WebTarget")
            .field("addr", &self.addr)
            .field("guest_ip", &self.guest_ip)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(auth_method_name))
            .finish()
    }
}

fn auth_method_name(a: &WebAuthSpec) -> &'static str {
    match a {
        WebAuthSpec::Basic { .. } => "basic",
        WebAuthSpec::Bearer { .. } => "bearer",
        WebAuthSpec::Header { .. } => "header",
        WebAuthSpec::Ntlm { .. } => "ntlm",
        WebAuthSpec::Form { .. } => "form",
    }
}

/// Per-page auth state guarded by a mutex during handshakes/logins.
#[derive(Default)]
enum AuthSession {
    #[default]
    None,
    /// NTLM connection is currently authenticated (best-effort hint).
    NtlmReady,
    /// Form login succeeded; cookies live in the client jar.
    FormLoggedIn,
    /// Last form-login attempt time, to rate-limit retries.
    FormFailed(Instant),
}

impl WebTarget {
    fn build(
        addr: std::net::SocketAddr,
        guest_ip: String,
        port: u16,
        auth: Option<WebAuthSpec>,
        machine: &str,
    ) -> Result<Arc<Self>, String> {
        let mut b = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // The guest sees its own lab-DNS hostname as Host, so absolute
            // URLs it emits are rewritable; the connection is pinned to the
            // loopback forward.
            .resolve(machine, addr);
        match &auth {
            Some(WebAuthSpec::Ntlm { .. }) => {
                // NTLM is connection-oriented: keep one authenticated h1
                // connection warm.
                b = b
                    .http1_only()
                    .pool_max_idle_per_host(1)
                    .pool_idle_timeout(None);
            }
            Some(WebAuthSpec::Form { .. }) => {
                b = b.cookie_store(true);
            }
            _ => {}
        }
        let client = b.build().map_err(|e| format!("client build failed: {e}"))?;
        Ok(Arc::new(WebTarget {
            addr,
            guest_ip,
            port,
            auth,
            client,
            host: machine.to_string(),
            session: Mutex::new(AuthSession::default()),
        }))
    }

    /// Authorities that count as "the guest itself" for URL rewriting.
    fn self_authorities(&self, machine: &str) -> Vec<String> {
        vec![
            format!("{machine}:{}", self.port),
            machine.to_string(),
            format!("{}:{}", self.guest_ip, self.port),
            self.guest_ip.clone(),
        ]
    }
}

// ---- session cookie endpoint ------------------------------------------------

/// `POST /api/web/session` — mint the path-scoped cookie the iframe proxy
/// reads (the request is already gated by the `/api/` auth middleware). No
/// cookie needed when auth is disabled.
pub async fn web_session(state: web::Data<AppState>, req: HttpRequest) -> HttpResponse {
    if !state.auth.enabled {
        return HttpResponse::Ok().json(json!({"ok": true}));
    }
    let Some(token) = super::auth::request_token(&req) else {
        return HttpResponse::Unauthorized().json(json!({"error": "not authenticated"}));
    };
    // Path-scoped, HttpOnly: rides iframe subresource requests automatically.
    let cookie = format!("{WEB_COOKIE}={token}; Path=/web; HttpOnly; SameSite=Strict");
    HttpResponse::Ok()
        .insert_header((header::SET_COOKIE, cookie))
        .json(json!({"ok": true}))
}

// ---- proxy handler ----------------------------------------------------------

/// No-tail form (`…/{page}`) → redirect to the trailing slash so relative
/// URLs in the page resolve under the prefix.
pub async fn proxy_root(
    req: HttpRequest,
    path: web::Path<(String, String, String, String)>,
) -> HttpResponse {
    let (lab, kind, machine, page) = path.into_inner();
    let mut loc = format!(
        "/web/{}/{}/{}/{}/",
        enc(&lab),
        enc(&kind),
        enc(&machine),
        enc(&page)
    );
    if let Some(q) = non_empty_query(&req) {
        loc.push('?');
        loc.push_str(q);
    }
    HttpResponse::TemporaryRedirect()
        .insert_header((header::LOCATION, loc))
        .finish()
}

/// `/web/{lab}/{kind}/{machine}/{page}/{tail:.*}` — the actual proxy.
pub async fn proxy(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<(String, String, String, String, String)>,
    body: web::Bytes,
) -> HttpResponse {
    let (lab, kind, machine, page, tail) = path.into_inner();

    // Console-auth cookie (only when auth is enabled).
    if state.auth.enabled && !web_cookie_ok(&state, &req).await {
        return HttpResponse::Unauthorized()
            .body("web session expired — reload the console and log in again");
    }
    if !valid_name(&lab) || !valid_name(&machine) || !valid_name(&page) {
        return HttpResponse::BadRequest().body("invalid lab/machine/page name");
    }
    if kind != "vms" && kind != "containers" {
        return HttpResponse::BadRequest().body("kind must be vms or containers");
    }
    if body.len() > MAX_BODY {
        return HttpResponse::PayloadTooLarge().body("request body too large");
    }

    let key = (lab.clone(), kind.clone(), machine.clone(), page.clone());
    let target = match resolve_target(&state, &key, &machine).await {
        Ok(t) => t,
        Err(e) => return HttpResponse::BadGateway().body(e),
    };

    let prefix = format!(
        "/web/{}/{}/{}/{}",
        enc(&lab),
        enc(&kind),
        enc(&machine),
        enc(&page)
    );
    let query = non_empty_query(&req).map(str::to_string);

    match forward(
        &target,
        &machine,
        &req,
        &tail,
        query.as_deref(),
        body.clone(),
        &prefix,
    )
    .await
    {
        Ok(resp) => resp,
        Err(ForwardError::Connect) => {
            // Forward may be stale (machine restarted → new lease). Drop the
            // cache, re-resolve once, retry.
            state.invalidate_web_target(&key).await;
            match resolve_target(&state, &key, &machine).await {
                Ok(t2) => {
                    match forward(&t2, &machine, &req, &tail, query.as_deref(), body, &prefix).await
                    {
                        Ok(resp) => resp,
                        Err(_) => HttpResponse::BadGateway()
                            .body("web page unreachable (guest not responding)"),
                    }
                }
                Err(e) => HttpResponse::BadGateway().body(e),
            }
        }
        Err(ForwardError::Other(msg)) => HttpResponse::BadGateway().body(msg),
    }
}

async fn web_cookie_ok(state: &AppState, req: &HttpRequest) -> bool {
    let Some(cookies) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    match cookie_value(cookies, WEB_COOKIE) {
        Some(tok) => state.valid_session(&tok).await,
        None => false,
    }
}

/// Cached target, or a fresh `web.forward` → built `WebTarget`.
async fn resolve_target(
    state: &AppState,
    key: &(String, String, String, String),
    machine: &str,
) -> Result<Arc<WebTarget>, String> {
    if let Some(t) = state.web_target(key).await {
        return Ok(t);
    }
    let (lab, _kind, _machine, page) = key;
    let reply = state
        .lab_call(
            lab,
            "web.forward",
            json!({"machine": machine, "page": page}),
        )
        .await?;
    let addr: std::net::SocketAddr = reply["addr"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or("web.forward returned no address")?;
    let guest_ip = reply["guest_ip"].as_str().unwrap_or("").to_string();
    let port = reply["port"].as_u64().unwrap_or(0) as u16;
    let auth: Option<WebAuthSpec> = reply
        .get("auth")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let target = WebTarget::build(addr, guest_ip, port, auth, machine)?;
    state.set_web_target(key.clone(), target.clone()).await;
    Ok(target)
}

enum ForwardError {
    /// Connection-level failure — worth one re-resolve+retry.
    Connect,
    Other(String),
}

/// Forward a single request to the guest, applying upstream auth, and map
/// the response back under the page prefix.
async fn forward(
    target: &Arc<WebTarget>,
    machine: &str,
    req: &HttpRequest,
    tail: &str,
    query: Option<&str>,
    body: web::Bytes,
    prefix: &str,
) -> Result<HttpResponse, ForwardError> {
    let mut url = format!("http://{machine}:{}/{tail}", target.port);
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .map_err(|e| ForwardError::Other(format!("bad method: {e}")))?;

    let headers = upstream_headers(req);
    let resp = authorize_and_send(target, &method, &url, &headers, &body).await?;
    Ok(map_response(target, machine, prefix, resp).await)
}

/// Build the header set forwarded upstream: clone the incoming ones, drop
/// hop-by-hop / rewritten ones, strip the console cookie and any
/// browser-supplied Authorization (ours is injected per method), force
/// identity encoding so bodies are rewritable.
fn upstream_headers(req: &HttpRequest) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in req.headers() {
        let n = name.as_str().to_ascii_lowercase();
        if matches!(
            n.as_str(),
            "host" | "accept-encoding" | "connection" | "content-length" | "authorization"
        ) {
            continue;
        }
        if n == "cookie" {
            // Strip our console cookie; pass any others (the guest's own).
            if let Ok(s) = value.to_str() {
                let kept: Vec<&str> = s
                    .split(';')
                    .map(str::trim)
                    .filter(|kv| !kv.starts_with(&format!("{WEB_COOKIE}=")))
                    .collect();
                if !kept.is_empty()
                    && let Ok(hv) = reqwest::header::HeaderValue::from_str(&kept.join("; "))
                {
                    out.insert(reqwest::header::COOKIE, hv);
                }
            }
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.insert(hn, hv);
        }
    }
    out.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    out
}

/// Send the request with the page's upstream auth applied.
async fn authorize_and_send(
    target: &Arc<WebTarget>,
    method: &reqwest::Method,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    body: &web::Bytes,
) -> Result<reqwest::Response, ForwardError> {
    let send = |extra: Option<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>| {
        let mut rb = target
            .client
            .request(method.clone(), url)
            .headers(headers.clone())
            .body(body.to_vec());
        if let Some((n, v)) = extra {
            rb = rb.header(n, v);
        }
        rb.send()
    };

    match &target.auth {
        None => send_mapped(send(None)).await,
        Some(WebAuthSpec::Basic { username, password }) => {
            let v = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            );
            send_mapped(send(Some((reqwest::header::AUTHORIZATION, hv(&v)?)))).await
        }
        Some(WebAuthSpec::Bearer { token }) => {
            let v = format!("Bearer {token}");
            send_mapped(send(Some((reqwest::header::AUTHORIZATION, hv(&v)?)))).await
        }
        Some(WebAuthSpec::Header { name, value }) => {
            let n = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| ForwardError::Other(format!("bad auth header name: {e}")))?;
            send_mapped(send(Some((n, hv(value)?)))).await
        }
        Some(WebAuthSpec::Ntlm { .. }) => ntlm_send(target, method, url, headers, body).await,
        Some(WebAuthSpec::Form { .. }) => form_send(target, send).await,
    }
}

fn hv(s: &str) -> Result<reqwest::header::HeaderValue, ForwardError> {
    reqwest::header::HeaderValue::from_str(s)
        .map_err(|e| ForwardError::Other(format!("bad header value: {e}")))
}

async fn send_mapped(
    fut: impl std::future::Future<Output = reqwest::Result<reqwest::Response>>,
) -> Result<reqwest::Response, ForwardError> {
    fut.await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            ForwardError::Connect
        } else {
            ForwardError::Other(format!("upstream request failed: {e}"))
        }
    })
}

// ---- NTLM (IIS / AD integrated auth) ----------------------------------------

/// Send a request negotiating NTLM on a 401. Serialized per page so the
/// 3-leg handshake owns the pooled connection; IIS keeps the connection
/// authenticated so warm requests skip the challenge.
async fn ntlm_send(
    target: &Arc<WebTarget>,
    method: &reqwest::Method,
    url: &str,
    headers: &reqwest::header::HeaderMap,
    body: &web::Bytes,
) -> Result<reqwest::Response, ForwardError> {
    let Some(WebAuthSpec::Ntlm {
        username,
        password,
        domain,
    }) = &target.auth
    else {
        unreachable!("ntlm_send on non-ntlm target");
    };

    let plain = |target: &Arc<WebTarget>| {
        target
            .client
            .request(method.clone(), url)
            .headers(headers.clone())
            .body(body.to_vec())
            .send()
    };

    // Warm path: the connection may already be authenticated.
    let first = send_mapped(plain(target)).await?;
    if first.status() != reqwest::StatusCode::UNAUTHORIZED || !accepts_ntlm(first.headers()) {
        return Ok(first);
    }
    // Drain the challenge body so the connection can be reused.
    let _ = first.bytes().await;

    let _guard = target.session.lock().await;
    let creds = ntlmclient::Credentials {
        username: username.clone(),
        password: password.clone(),
        domain: domain.clone().unwrap_or_default(),
    };

    for _attempt in 0..2 {
        // Leg 1: Type 1 negotiate.
        let nego_flags = ntlmclient::Flags::NEGOTIATE_UNICODE
            | ntlmclient::Flags::REQUEST_TARGET
            | ntlmclient::Flags::NEGOTIATE_NTLM
            | ntlmclient::Flags::NEGOTIATE_WORKSTATION_SUPPLIED;
        let nego = ntlmclient::Message::Negotiate(ntlmclient::NegotiateMessage {
            flags: nego_flags,
            supplied_domain: String::new(),
            supplied_workstation: "vmlab".to_string(),
            os_version: Default::default(),
        });
        let nego_b64 = match nego.to_bytes() {
            Ok(b) => base64::engine::general_purpose::STANDARD.encode(b),
            Err(_) => return Err(ForwardError::Other("ntlm negotiate encode failed".into())),
        };
        let chal_resp = send_mapped(
            target
                .client
                .request(method.clone(), url)
                .headers(headers.clone())
                .header(reqwest::header::AUTHORIZATION, format!("NTLM {nego_b64}"))
                .body(body.to_vec())
                .send(),
        )
        .await?;

        // Leg 2: read the Type 2 challenge.
        let Some(chal_b64) = chal_resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split_whitespace().nth(1).map(str::to_string))
        else {
            // Server dropped the negotiation — surface whatever it sent.
            return Ok(chal_resp);
        };
        let _ = chal_resp.bytes().await;
        let Ok(chal_bytes) = base64::engine::general_purpose::STANDARD.decode(&chal_b64) else {
            return Err(ForwardError::Other("ntlm challenge not base64".into()));
        };
        let challenge = match ntlmclient::Message::try_from(chal_bytes.as_slice()) {
            Ok(ntlmclient::Message::Challenge(c)) => c,
            _ => {
                // A fresh negotiation restart — try again.
                continue;
            }
        };
        let target_info: Vec<u8> = challenge
            .target_information
            .iter()
            .flat_map(|ie| ie.to_bytes())
            .collect();

        // Leg 3: Type 3 authenticate.
        let response = ntlmclient::respond_challenge_ntlm_v2(
            challenge.challenge,
            &target_info,
            ntlmclient::get_ntlm_time(),
            &creds,
        );
        let auth_msg = response.to_message(
            &creds,
            "vmlab",
            ntlmclient::Flags::NEGOTIATE_UNICODE | ntlmclient::Flags::NEGOTIATE_NTLM,
        );
        let auth_b64 = match auth_msg.to_bytes() {
            Ok(b) => base64::engine::general_purpose::STANDARD.encode(b),
            Err(_) => {
                return Err(ForwardError::Other(
                    "ntlm authenticate encode failed".into(),
                ));
            }
        };
        let authed = send_mapped(
            target
                .client
                .request(method.clone(), url)
                .headers(headers.clone())
                .header(reqwest::header::AUTHORIZATION, format!("NTLM {auth_b64}"))
                .body(body.to_vec())
                .send(),
        )
        .await?;
        if authed.status() != reqwest::StatusCode::UNAUTHORIZED {
            *target.session.lock().await = AuthSession::NtlmReady;
            return Ok(authed);
        }
        // 401 again → the connection was likely rotated; retry once.
    }
    // Give up: re-send plainly so the caller sees the guest's 401.
    send_mapped(plain(target)).await
}

fn accepts_ntlm(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::WWW_AUTHENTICATE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("ntlm") || v.contains("negotiate")
        })
}

// ---- form login (cookie capture) --------------------------------------------

async fn form_send<F, Fut>(
    target: &Arc<WebTarget>,
    send: F,
) -> Result<reqwest::Response, ForwardError>
where
    F: Fn(Option<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>) -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let Some(WebAuthSpec::Form { fail_redirect, .. }) = &target.auth else {
        unreachable!("form_send on non-form target");
    };

    // Ensure we've logged in at least once.
    ensure_form_login(target, false).await?;
    let resp = send_mapped(send(None)).await?;
    if !form_needs_login(&resp, fail_redirect.as_deref()) {
        return Ok(resp);
    }
    // Session expired → re-login (rate-limited) and retry once.
    let _ = resp.bytes().await;
    if ensure_form_login(target, true).await? {
        send_mapped(send(None)).await
    } else {
        // Cooldown active or login failed — resend so the caller sees it.
        send_mapped(send(None)).await
    }
}

fn form_needs_login(resp: &reqwest::Response, fail_redirect: Option<&str>) -> bool {
    let s = resp.status();
    if s == reqwest::StatusCode::UNAUTHORIZED || s == reqwest::StatusCode::FORBIDDEN {
        return true;
    }
    if s.is_redirection()
        && let Some(fr) = fail_redirect
        && let Some(loc) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
    {
        return loc.contains(fr);
    }
    false
}

/// Perform the configured login request, capturing cookies in the jar.
/// Returns whether a login actually ran. `force` re-logins even if a prior
/// one succeeded (subject to the cooldown).
async fn ensure_form_login(target: &Arc<WebTarget>, force: bool) -> Result<bool, ForwardError> {
    let Some(WebAuthSpec::Form {
        username,
        password,
        login_path,
        login_method,
        login_body,
        login_content_type,
        ..
    }) = &target.auth
    else {
        unreachable!("ensure_form_login on non-form target");
    };
    let mut sess = target.session.lock().await;
    match &*sess {
        AuthSession::FormLoggedIn if !force => return Ok(false),
        AuthSession::FormFailed(t) if t.elapsed() < RELOGIN_COOLDOWN => return Ok(false),
        _ => {}
    }
    let url = format!("http://{}:{}{}", target.host, target.port, login_path);
    let body = login_body
        .replace("{user}", &encode_for(login_content_type, username))
        .replace("{pass}", &encode_for(login_content_type, password));
    let method =
        reqwest::Method::from_bytes(login_method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let resp = target
        .client
        .request(method, &url)
        .header(reqwest::header::CONTENT_TYPE, login_content_type.clone())
        .body(body)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => {
            *sess = AuthSession::FormLoggedIn;
            Ok(true)
        }
        _ => {
            *sess = AuthSession::FormFailed(Instant::now());
            Ok(false)
        }
    }
}

fn encode_for(content_type: &str, v: &str) -> String {
    if content_type.contains("json") {
        // Escape for embedding inside a JSON string literal.
        v.replace('\\', "\\\\").replace('"', "\\\"")
    } else {
        form_urlencode(v)
    }
}

fn form_urlencode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- response mapping -------------------------------------------------------

const DROP_HEADERS: &[&str] = &[
    "x-frame-options",
    "content-security-policy",
    "content-security-policy-report-only",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "strict-transport-security",
];

async fn map_response(
    target: &Arc<WebTarget>,
    machine: &str,
    prefix: &str,
    resp: reqwest::Response,
) -> HttpResponse {
    let status = resp.status();
    let src_headers = resp.headers().clone();
    let content_type = src_headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_form = matches!(target.auth, Some(WebAuthSpec::Form { .. }));

    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return HttpResponse::BadGateway().body(format!("upstream read failed: {e}")),
    };

    let authorities = target.self_authorities(machine);
    let out_body: Vec<u8> = if content_type.contains("text/html") {
        rewrite_html(&String::from_utf8_lossy(&raw), prefix, &authorities).into_bytes()
    } else if content_type.contains("text/css") {
        rewrite_css(&String::from_utf8_lossy(&raw), prefix, &authorities).into_bytes()
    } else {
        raw.to_vec()
    };

    let mut builder = HttpResponse::build(
        actix_web::http::StatusCode::from_u16(status.as_u16())
            .unwrap_or(actix_web::http::StatusCode::BAD_GATEWAY),
    );
    for (name, value) in src_headers.iter() {
        let n = name.as_str().to_ascii_lowercase();
        if DROP_HEADERS.contains(&n.as_str()) {
            continue;
        }
        // Form auth: the jar owns the guest session; passing its Set-Cookie
        // through would bleed guest cookies across the console origin.
        if is_form && n == "set-cookie" {
            continue;
        }
        if n == "location"
            && let Ok(v) = value.to_str()
        {
            if let Some(mapped) = map_url(v, prefix, &authorities) {
                builder.insert_header((header::LOCATION, mapped));
            } else {
                builder.insert_header((header::LOCATION, v));
            }
            continue;
        }
        if let Ok(v) = value.to_str() {
            builder.insert_header((name.as_str(), v));
        }
    }
    builder.body(out_body)
}

// ---- URL rewriting (adapted from aciddog preview.rs) ------------------------

/// Map one URL from the guest page under the page prefix. `None` = leave it
/// (relative URLs already resolve; foreign absolute URLs are out of scope).
fn map_url(v: &str, prefix: &str, authorities: &[String]) -> Option<String> {
    let vt = v.trim();
    if vt.is_empty() {
        return None;
    }
    let low = vt.to_ascii_lowercase();
    for skip in [
        "data:",
        "mailto:",
        "javascript:",
        "tel:",
        "blob:",
        "about:",
        "#",
    ] {
        if low.starts_with(skip) {
            return None;
        }
    }
    // Absolute (http/https) or protocol-relative → only rewrite when it
    // points at the guest itself; foreign origins are left alone.
    if low.starts_with("http://") || low.starts_with("https://") || vt.starts_with("//") {
        let after_scheme = vt
            .strip_prefix("http://")
            .or_else(|| vt.strip_prefix("https://"))
            .or_else(|| vt.strip_prefix("//"))?;
        let (authority, rest) = match after_scheme.find('/') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, "/"),
        };
        if authorities.iter().any(|a| a == authority) {
            return Some(format!("{prefix}{rest}"));
        }
        return None;
    }
    if vt.starts_with('/') {
        return Some(format!("{prefix}{vt}"));
    }
    None
}

fn attr_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(href|src|action|poster|formaction|data)\s*=\s*("|')([^"']*)("|')"#)
            .unwrap()
    })
}

fn srcset_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)srcset\s*=\s*("|')([^"']*)("|')"#).unwrap())
}

fn css_url_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)url\(\s*("|'|)([^"')]+)("|'|)\s*\)"#).unwrap())
}

fn rewrite_html(html: &str, prefix: &str, authorities: &[String]) -> String {
    let step1 = attr_re().replace_all(html, |c: &regex::Captures| {
        let attr = &c[1];
        let q = &c[2];
        let val = &c[3];
        match map_url(val, prefix, authorities) {
            Some(mapped) => format!("{attr}={q}{mapped}{q}"),
            None => c[0].to_string(),
        }
    });
    let step2 = srcset_re().replace_all(&step1, |c: &regex::Captures| {
        let q = &c[1];
        let list = &c[2];
        let mapped: Vec<String> = list
            .split(',')
            .map(|item| {
                let item = item.trim();
                let mut parts = item.splitn(2, char::is_whitespace);
                let url = parts.next().unwrap_or("");
                let desc = parts.next().unwrap_or("");
                let u = map_url(url, prefix, authorities).unwrap_or_else(|| url.to_string());
                if desc.is_empty() {
                    u
                } else {
                    format!("{u} {desc}")
                }
            })
            .collect();
        format!("srcset={q}{}{q}", mapped.join(", "))
    });
    rewrite_css(&step2, prefix, authorities)
}

fn rewrite_css(css: &str, prefix: &str, authorities: &[String]) -> String {
    css_url_re()
        .replace_all(css, |c: &regex::Captures| {
            let q = &c[1];
            let url = &c[2];
            match map_url(url, prefix, authorities) {
                Some(mapped) => format!("url({q}{mapped}{q})"),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

// ---- small helpers ----------------------------------------------------------

fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn enc(s: &str) -> String {
    // Path segments are validated DNS labels / known kinds — no escaping
    // needed, but keep the call site honest.
    s.to_string()
}

fn non_empty_query(req: &HttpRequest) -> Option<&str> {
    let q = req.query_string();
    (!q.is_empty()).then_some(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AuthConfig;
    use actix_web::{App, test};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // ---- pure rewriter tests ----

    #[actix_web::test]
    async fn map_url_rewrites_self_and_root_leaves_foreign() {
        let prefix = "/web/lab/vms/app/ui";
        let auth = vec!["app:3000".to_string(), "app".to_string()];
        assert_eq!(
            map_url("/assets/x.css", prefix, &auth),
            Some(format!("{prefix}/assets/x.css"))
        );
        assert_eq!(
            map_url("http://app:3000/a", prefix, &auth),
            Some(format!("{prefix}/a"))
        );
        // Foreign origin: left alone.
        assert_eq!(map_url("http://cdn.example/x.js", prefix, &auth), None);
        // Relative + non-navigable: left alone.
        assert_eq!(map_url("img/logo.png", prefix, &auth), None);
        for u in ["data:x", "mailto:a@b", "javascript:void(0)", "#top", ""] {
            assert_eq!(map_url(u, prefix, &auth), None, "{u}");
        }
    }

    #[actix_web::test]
    async fn rewrite_html_and_css_stay_under_prefix() {
        let prefix = "/web/l/vms/a/ui";
        let auth = vec!["a".to_string()];
        let html = r#"<a href="/x">x</a><img srcset="/a.png 1x, http://cdn/b.png 2x">"#;
        let out = rewrite_html(html, prefix, &auth);
        assert!(out.contains(r#"href="/web/l/vms/a/ui/x""#), "{out}");
        assert!(out.contains("/web/l/vms/a/ui/a.png 1x"), "{out}");
        assert!(out.contains("http://cdn/b.png 2x"), "{out}"); // foreign kept
        let css = "body { background: url('/bg.png'); }";
        assert!(rewrite_css(css, prefix, &auth).contains("url('/web/l/vms/a/ui/bg.png')"));
    }

    // ---- fake guest + proxy integration ----

    /// Spawn a one-request-per-connection HTTP/1.1 responder on 127.0.0.1:0.
    /// `handler(request_text) -> raw_response_bytes`. Returns the bound addr.
    async fn fake_guest<F>(handler: F) -> std::net::SocketAddr
    where
        F: Fn(String) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let resp = handler(req);
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                });
            }
        });
        addr
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut s = format!("HTTP/1.1 {status}\r\n");
        for (k, v) in headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str(&format!("Content-Length: {}\r\n", body.len()));
        s.push_str("Connection: close\r\n\r\n");
        s.push_str(body);
        s.into_bytes()
    }

    fn state_with(auth_enabled: bool) -> web::Data<AppState> {
        web::Data::new(AppState::new(
            AuthConfig {
                enabled: auth_enabled,
                user: "u".into(),
                password_hash: String::new(),
            },
            Some(("lab".into(), std::env::temp_dir())),
            false,
        ))
    }

    async fn seed(
        state: &web::Data<AppState>,
        addr: std::net::SocketAddr,
        auth: Option<WebAuthSpec>,
    ) {
        let target = WebTarget::build(addr, "10.0.0.5".into(), addr.port(), auth, "app").unwrap();
        state
            .set_web_target(
                ("lab".into(), "vms".into(), "app".into(), "ui".into()),
                target,
            )
            .await;
    }

    macro_rules! proxy_app {
        ($state:expr) => {
            test::init_service(App::new().app_data($state.clone()).route(
                "/web/{lab}/{kind}/{machine}/{page}/{tail:.*}",
                web::route().to(proxy),
            ))
            .await
        };
    }

    #[actix_web::test]
    async fn proxy_strips_frame_headers_and_rewrites_body() {
        let addr = fake_guest(|_req| {
            http_response(
                "200 OK",
                &[
                    ("Content-Type", "text/html"),
                    ("X-Frame-Options", "DENY"),
                    ("Content-Security-Policy", "default-src 'none'"),
                ],
                r#"<a href="/dash">d</a>"#,
            )
        })
        .await;
        let state = state_with(false);
        seed(&state, addr, None).await;
        let app = proxy_app!(state);
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/web/lab/vms/app/ui/")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-frame-options").is_none());
        assert!(resp.headers().get("content-security-policy").is_none());
        let body = test::read_body(resp).await;
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains(r#"href="/web/lab/vms/app/ui/dash""#),
            "{text}"
        );
    }

    #[actix_web::test]
    async fn proxy_requires_cookie_when_auth_enabled() {
        let addr = fake_guest(|_r| http_response("200 OK", &[], "ok")).await;
        let state = state_with(true);
        seed(&state, addr, None).await;
        let app = proxy_app!(state);

        // No cookie → 401.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/web/lab/vms/app/ui/")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 401);

        // Valid session cookie → 200.
        state.create_session("tok123".into()).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/web/lab/vms/app/ui/")
                .insert_header((header::COOKIE, "vmlab_web=tok123"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn proxy_injects_basic_auth_and_strips_browser_auth_and_cookie() {
        let addr = fake_guest(|req| {
            // Echo the Authorization header the guest received.
            let auth = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .unwrap_or("authorization: (none)")
                .to_string();
            let cookie = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                .unwrap_or("cookie: (none)")
                .to_string();
            http_response("200 OK", &[], &format!("{auth}\n{cookie}"))
        })
        .await;
        let state = state_with(false);
        seed(
            &state,
            addr,
            Some(WebAuthSpec::Basic {
                username: "admin".into(),
                password: "pw".into(),
            }),
        )
        .await;
        let app = proxy_app!(state);
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/web/lab/vms/app/ui/")
                // A browser-supplied Authorization + our console cookie must
                // not reach the guest.
                .insert_header((header::AUTHORIZATION, "Bearer browser-token"))
                .insert_header((header::COOKIE, "vmlab_web=x; other=keep"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = test::read_body(resp).await;
        let text = String::from_utf8_lossy(&body);
        let expected = base64::engine::general_purpose::STANDARD.encode("admin:pw");
        assert!(text.contains(&format!("Basic {expected}")), "{text}");
        assert!(!text.contains("browser-token"), "{text}");
        assert!(!text.contains("vmlab_web"), "{text}");
        assert!(text.contains("other=keep"), "{text}");
    }

    #[actix_web::test]
    async fn proxy_form_login_captures_and_replays_cookie() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let logins = Arc::new(AtomicUsize::new(0));
        let l2 = logins.clone();
        let addr = fake_guest(move |req| {
            let first_line = req.lines().next().unwrap_or("").to_string();
            if first_line.starts_with("POST /login") {
                l2.fetch_add(1, Ordering::SeqCst);
                // Validate the substituted body carried the creds.
                assert!(req.contains("user=admin"), "login body: {req}");
                assert!(req.contains("password=pw"), "login body: {req}");
                return http_response("200 OK", &[("Set-Cookie", "sess=abc; Path=/")], "ok");
            }
            // Protected page: 401 unless the session cookie is present.
            let has_cookie = req
                .lines()
                .any(|l| l.to_ascii_lowercase().starts_with("cookie:") && l.contains("sess=abc"));
            if has_cookie {
                http_response("200 OK", &[], "secret")
            } else {
                http_response("401 Unauthorized", &[], "denied")
            }
        })
        .await;
        let state = state_with(false);
        seed(
            &state,
            addr,
            Some(WebAuthSpec::Form {
                username: "admin".into(),
                password: "pw".into(),
                login_path: "/login".into(),
                login_method: "POST".into(),
                login_body: "user={user}&password={pass}".into(),
                login_content_type: "application/x-www-form-urlencoded".into(),
                fail_redirect: None,
            }),
        )
        .await;
        let app = proxy_app!(state);
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/web/lab/vms/app/ui/")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = test::read_body(resp).await;
        assert_eq!(String::from_utf8_lossy(&body), "secret");
        assert_eq!(logins.load(Ordering::SeqCst), 1);
        // The guest's Set-Cookie must not leak to the browser (jar owns it).
        // (Second request reuses the jar; no re-login.)
    }

    #[actix_web::test]
    async fn web_session_sets_scoped_cookie() {
        let state = state_with(true);
        state.create_session("tok".into()).await;
        let app = test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/api/web/session", web::post().to(web_session)),
        )
        .await;
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/web/session")
                .insert_header((header::AUTHORIZATION, "Bearer tok"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.contains("vmlab_web=tok"), "{cookie}");
        assert!(cookie.contains("Path=/web"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
    }
}
