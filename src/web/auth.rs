//! Login, logout, and the bearer-token gate.
//!
//! When auth is disabled (the default loopback case) the middleware is a
//! pass-through and the SPA never shows a login screen. When enabled, the user
//! signs in with username + password (argon2-verified); a successful login
//! mints a random session token that guards `/api/*` (except the login/probe
//! endpoints) and `/vnc/*`.

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

/// Pull the bearer token from the `Authorization` header or a `?token=` query
/// param (WebSocket upgrades and `<img>` loads can't set headers).
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
        && (path.starts_with("/vnc/")
            || (path.starts_with("/api/") && path != "/api/login" && path != "/api/auth"));

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
    let addr = req.peer_addr().map(|a| a.ip());
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
