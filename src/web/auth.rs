//! Login, logout, and the bearer-token gate.
//!
//! When auth is disabled (the default loopback case) the middleware is a
//! pass-through and the SPA never shows a login screen. When enabled, the user
//! signs in with username + password (argon2-verified); a successful login
//! mints a random session token that guards `/api/*` (except the login/probe
//! endpoints), including the desktop console WebSocket.

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{Error, HttpRequest, HttpResponse, web};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine as _;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;

use super::state::AppState;

/// Hash a plaintext password into a PHC string (used once at startup when the
/// operator passes `--password`).
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("hashing password: {e}"))
}

fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The address login backoff should attribute a request to: the TCP peer,
/// or — only when the operator opted in with `--trust-proxy` — the *last*
/// entry of `X-Forwarded-For` (the one appended by the nearest proxy; earlier
/// entries are whatever the client sent).
fn client_ip(req: &HttpRequest, trust_proxy: bool) -> Option<std::net::IpAddr> {
    if trust_proxy
        && let Some(xff) = req.headers().get("x-forwarded-for")
        && let Ok(s) = xff.to_str()
        && let Some(ip) = s.rsplit(',').next().and_then(|e| e.trim().parse().ok())
    {
        return Some(ip);
    }
    req.peer_addr().map(|a| a.ip())
}

/// Pull the bearer token from the `Authorization` header or a `?token=` query
/// param (WebSocket upgrades and `<img>` loads can't set headers). Note the
/// query form means tokens can show up in reverse-proxy access logs; scrub
/// query strings there if that matters in your deployment.
pub fn request_token(req: &HttpRequest) -> Option<String> {
    if let Some(h) = req.headers().get("authorization")
        && let Ok(s) = h.to_str()
        && let Some(t) = s.strip_prefix("Bearer ")
    {
        return Some(t.to_string());
    }
    // Tokens are URL-safe base64 (no characters that need percent-decoding).
    req.query_string()
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(str::to_string)
}

/// Middleware gating protected routes when auth is enabled.
pub async fn gate(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let state = req
        .app_data::<web::Data<AppState>>()
        .expect("AppState present")
        .clone();

    let path = req.path();
    let protected = state.auth.enabled
        && path.starts_with("/api/")
        && path != "/api/login"
        && path != "/api/auth";

    if protected {
        let ok = match request_token(req.request()) {
            Some(t) => state.valid_session(&t).await,
            None => false,
        };
        if !ok {
            let (req, _pl) = req.into_parts();
            let resp = HttpResponse::Unauthorized()
                .json(json!({"error": "authentication required"}))
                .map_into_right_body();
            return Ok(ServiceResponse::new(req, resp));
        }
    }
    next.call(req).await.map(|r| r.map_into_left_body())
}

// --- handlers -------------------------------------------------------------

/// Reports whether the client must log in, and (if so) the configured user.
pub async fn probe(state: web::Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "auth_required": state.auth.enabled,
        "user": if state.auth.enabled { Some(state.auth.user.clone()) } else { None },
    }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    username: String,
    password: String,
}

pub async fn login(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<LoginBody>,
) -> HttpResponse {
    if !state.auth.enabled {
        // Auth off: hand back a throwaway token so the SPA flow is uniform.
        let token = new_token();
        state.create_session(token.clone()).await;
        return HttpResponse::Ok().json(json!({"token": token}));
    }
    // Per-address backoff: argon2 alone still allows an online brute force.
    let addr = client_ip(&req, state.trust_proxy);
    if let Some(addr) = addr
        && state.login_throttled(addr).await
    {
        return HttpResponse::TooManyRequests()
            .json(json!({"error": "too many failed logins; try again shortly"}));
    }
    let ok = body.username == state.auth.user
        && verify_password(&state.auth.password_hash, &body.password);
    if !ok {
        if let Some(addr) = addr {
            state.login_failed(addr).await;
        }
        return HttpResponse::Unauthorized().json(json!({"error": "invalid credentials"}));
    }
    if let Some(addr) = addr {
        state.login_succeeded(addr).await;
    }
    let token = new_token();
    state.create_session(token.clone()).await;
    HttpResponse::Ok().json(json!({"token": token}))
}

pub async fn logout(state: web::Data<AppState>, req: HttpRequest) -> HttpResponse {
    if let Some(t) = request_token(&req) {
        state.drop_session(&t).await;
    }
    HttpResponse::Ok().json(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use actix_web::middleware::from_fn;
    use actix_web::{App, HttpResponse, test, web};

    use super::super::state::{AppState, AuthConfig};
    use super::*;

    async fn protected() -> HttpResponse {
        HttpResponse::Ok().json(json!({"secret": true}))
    }

    /// A test app with the real gate, probe, and login handlers plus a
    /// protected API route and a protected console-WebSocket-style route.
    macro_rules! gated_app {
        ($state:expr) => {
            test::init_service(
                App::new()
                    .app_data($state.clone())
                    .wrap(from_fn(gate))
                    .route("/api/auth", web::get().to(probe))
                    .route("/api/login", web::post().to(login))
                    .route("/api/labs", web::get().to(protected))
                    .route("/api/desktop/vnc/lab/vm", web::get().to(protected)),
            )
            .await
        };
    }

    fn auth_state() -> web::Data<AppState> {
        web::Data::new(AppState::new(
            AuthConfig {
                enabled: true,
                user: "admin".into(),
                password_hash: hash_password("hunter2").unwrap(),
            },
            None,
            false,
        ))
    }

    #[actix_web::test]
    async fn gate_blocks_protected_routes_without_a_token() {
        let app = gated_app!(auth_state());
        for path in ["/api/labs", "/api/desktop/vnc/lab/vm"] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
            assert_eq!(resp.status(), 401, "{path}");
        }
        // Garbage bearer token is rejected too.
        let req = test::TestRequest::get()
            .uri("/api/labs")
            .insert_header(("authorization", "Bearer not-a-real-token"))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 401);
    }

    #[actix_web::test]
    async fn gate_exempts_probe_and_login_only() {
        let app = gated_app!(auth_state());
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/api/auth").to_request()).await;
        assert_eq!(resp.status(), 200);
        // Login is reachable without a token (that's the point) — bad creds
        // get 401 from the handler, not from the gate.
        let req = test::TestRequest::post()
            .uri("/api/login")
            .set_json(json!({"username": "admin", "password": "wrong"}))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 401);
    }

    #[actix_web::test]
    async fn login_token_unlocks_header_and_query_access() {
        let app = gated_app!(auth_state());
        let req = test::TestRequest::post()
            .uri("/api/login")
            .set_json(json!({"username": "admin", "password": "hunter2"}))
            .to_request();
        let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
        let token = body["token"].as_str().expect("token issued").to_string();

        let req = test::TestRequest::get()
            .uri("/api/labs")
            .insert_header(("authorization", format!("Bearer {token}")))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);

        // WebSocket upgrades / <img> loads use the query-param form.
        let req = test::TestRequest::get()
            .uri(&format!("/api/desktop/vnc/lab/vm?token={token}"))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);
    }

    #[actix_web::test]
    async fn disabled_auth_is_a_passthrough() {
        let state = web::Data::new(AppState::new(
            AuthConfig {
                enabled: false,
                user: String::new(),
                password_hash: String::new(),
            },
            None,
            false,
        ));
        let app = gated_app!(state);
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/api/labs").to_request()).await;
        assert_eq!(resp.status(), 200);
    }
}
