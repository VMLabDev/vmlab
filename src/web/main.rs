//! `vmlab-web` — an Actix-web server that exposes vmlab over a REST + WebSocket
//! API and serves the embedded SolidJS console UI. It talks to the same
//! supervisor and lab daemons the CLI does, over the existing unix-socket
//! protocol; no daemon changes are involved.
//!
//! Besides the flags below, `VMLAB_WEB_SESSION_TTL_SECS` overrides the idle
//! session lifetime (default 12 hours).

mod api;
mod assets;
mod auth;
mod desktop;
mod editor;
mod events;
mod files;
mod fsops;
mod help;
mod logs;
mod pkgs;
mod playbooks;
mod state;
mod tty;

use std::net::IpAddr;
use std::process::ExitCode;

use actix_web::middleware::from_fn;
use actix_web::{App, HttpServer, web};
use clap::Parser;

use state::{AppState, AuthConfig};

#[derive(Parser)]
#[command(name = "vmlab-web", version, about = "Web UI server for vmlab")]
struct Args {
    /// Address to bind (non-loopback implies --auth)
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// TCP port
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// Require username/password login (auto-enabled for non-loopback binds)
    #[arg(long)]
    auth: bool,
    /// Allow a non-loopback bind with no login (ignored if credentials are set)
    #[arg(long)]
    no_auth: bool,
    /// Login username (or VMLAB_WEB_USER)
    #[arg(long)]
    user: Option<String>,
    /// Login password, hashed once at startup (or VMLAB_WEB_PASSWORD; prefer a hash)
    #[arg(long)]
    password: Option<String>,
    /// Pre-computed argon2 PHC password hash (or VMLAB_WEB_PASSWORD_HASH)
    #[arg(long)]
    password_hash: Option<String>,
    /// Bring the working-directory lab up on startup (or VMLAB_WEB_UP)
    #[arg(long)]
    up: bool,
    /// Behind a reverse proxy: attribute clients by the proxy-appended
    /// X-Forwarded-For entry for login rate limiting (or VMLAB_WEB_TRUST_PROXY)
    #[arg(long)]
    trust_proxy: bool,
}

/// A CLI flag value, falling back to an environment variable. Empty values
/// (e.g. an env var set to "" by a compose `${VAR:-}` default) count as unset.
fn or_env(flag: &Option<String>, var: &str) -> Option<String> {
    flag.clone()
        .or_else(|| std::env::var(var).ok())
        .filter(|s| !s.is_empty())
}

/// A boolean environment toggle: set and not falsey ("", "0", "false", "no").
fn env_flag(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "" | "0" | "false" | "no"))
        .unwrap_or(false)
}

fn build_auth(args: &Args) -> Result<AuthConfig, String> {
    let user = or_env(&args.user, "VMLAB_WEB_USER");
    let hash = or_env(&args.password_hash, "VMLAB_WEB_PASSWORD_HASH");
    let plain = or_env(&args.password, "VMLAB_WEB_PASSWORD");

    // Credentials win: if a username + a password/hash are supplied, enable auth
    // regardless of --no-auth.
    if let Some(user) = user
        && (hash.is_some() || plain.is_some())
    {
        let password_hash = match (hash, plain) {
            (Some(h), _) => h,
            (None, Some(p)) => auth::hash_password(&p)?,
            (None, None) => unreachable!(),
        };
        return Ok(AuthConfig {
            enabled: true,
            user,
            password_hash,
        });
    }
    if args.auth {
        return Err(
            "--auth requires --user + --password/--password-hash (or the \
                    VMLAB_WEB_USER / VMLAB_WEB_PASSWORD[_HASH] env vars)"
                .into(),
        );
    }

    // No credentials. Running open is allowed on a loopback bind, or anywhere
    // with an explicit --no-auth opt-in; otherwise refuse (secure default).
    if args.no_auth || args.bind.is_loopback() {
        return Ok(AuthConfig {
            enabled: false,
            user: String::new(),
            password_hash: String::new(),
        });
    }
    Err(
        "binding a non-loopback address with no login is refused by default — set \
         credentials (--user + --password/--password-hash, or the VMLAB_WEB_USER / \
         VMLAB_WEB_PASSWORD env vars) or pass --no-auth to opt in"
            .into(),
    )
}

#[actix_web::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let auth = match build_auth(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vmlab-web: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The lab in the working directory (if any) is the default; the switcher
    // also lists every lab the supervisor knows about.
    let default_lab = vmlab::cli::lab::current_lab().ok();
    match &default_lab {
        Some((name, root)) => println!("vmlab-web: default lab `{name}` ({})", root.display()),
        None => {
            println!("vmlab-web: no lab in the working directory (switcher lists running labs)")
        }
    }
    if auth.enabled {
        println!("vmlab-web: authentication enabled (user `{}`)", auth.user);
    } else if !args.bind.is_loopback() {
        println!(
            "vmlab-web: WARNING — no authentication on a non-loopback bind ({}); \
             anyone who can reach this port has full control of the labs",
            args.bind
        );
    }

    let trust_proxy = args.trust_proxy || env_flag("VMLAB_WEB_TRUST_PROXY");
    let data = web::Data::new(AppState::new(auth, default_lab, trust_proxy));
    let (bind, port) = (args.bind, args.port);

    // Optionally bring the working-directory lab up so it is already running
    // (or visibly booting) when the user opens the UI. Done in the background
    // so the server starts serving immediately — the lab's progress streams to
    // the events feed as VMs come up.
    if args.up || env_flag("VMLAB_WEB_UP") {
        match data.default_lab.clone() {
            Some((name, _)) => {
                let data = data.clone();
                actix_web::rt::spawn(async move {
                    println!("vmlab-web: bringing lab `{name}` up…");
                    match data.lab_call(&name, "up", serde_json::json!({})).await {
                        Ok(_) => println!("vmlab-web: lab `{name}` is up"),
                        Err(e) => eprintln!("vmlab-web: lab `{name}` failed to come up: {e}"),
                    }
                });
            }
            None => eprintln!(
                "vmlab-web: --up set but no lab in the working directory; nothing to start"
            ),
        }
    }

    println!("vmlab-web: listening on http://{bind}:{port}");

    let server = HttpServer::new(move || {
        App::new()
            .app_data(data.clone())
            .wrap(from_fn(auth::gate))
            // Auth (exempt from the gate).
            .route("/api/auth", web::get().to(auth::probe))
            .route("/api/login", web::post().to(auth::login))
            .route("/api/logout", web::post().to(auth::logout))
            // Labs.
            .route("/api/labs", web::get().to(api::list_labs))
            .route("/api/labs", web::post().to(api::create_lab))
            // Catalogs for the visual editor's pickers.
            .route(
                "/api/catalog/templates",
                web::get().to(api::catalog_templates),
            )
            .route(
                "/api/catalog/templates/{arch}/{name}/{version}",
                web::delete().to(api::delete_catalog_template),
            )
            .route(
                "/api/catalog/profiles",
                web::get().to(api::catalog_profiles),
            )
            .route("/api/catalog/meta", web::get().to(api::catalog_meta))
            .route("/api/catalog/oci", web::get().to(api::catalog_oci))
            .route("/api/registries", web::get().to(api::list_registries))
            .route("/api/registries", web::post().to(api::add_registry))
            .route("/api/registries", web::delete().to(api::remove_registry))
            .route("/api/registries/login", web::post().to(api::registry_login))
            // Host capacity + server-side file picker for the visual editor.
            .route("/api/host", web::get().to(api::host_info))
            .route("/api/host/fs", web::get().to(api::host_fs))
            // The daemon's network fast-path tier (Topbar badge).
            .route("/api/fastpath", web::get().to(api::fastpath))
            // VM sub-routes (literal before the `{action}` catch-all).
            .route(
                "/api/labs/{lab}/vms/{vm}/sendkeys",
                web::post().to(api::vm_sendkeys),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/screenshot.png",
                web::get().to(api::vm_screenshot),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/snapshots",
                web::get().to(api::vm_snapshots),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/snapshots/{name}",
                web::delete().to(api::snapshot_delete),
            )
            // Interactive terminal inside the VM (vmlab-agent session).
            .route("/api/labs/{lab}/vms/{vm}/tty", web::get().to(tty::vm_tty))
            // Guest metrics + clipboard (vmlab-agent).
            .route(
                "/api/labs/{lab}/vms/{vm}/stats",
                web::get().to(api::vm_stats),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/clipboard",
                web::get().to(api::vm_clipboard_get),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/clipboard",
                web::post().to(api::vm_clipboard_set),
            )
            .route(
                "/api/labs/{lab}/containers/{container}/stats",
                web::get().to(api::container_stats),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/{action}",
                web::post().to(api::vm_action),
            )
            // Container lifecycle (mirrors the VM actions).
            .route(
                "/api/labs/{lab}/containers/{container}/tty",
                web::get().to(tty::container_tty),
            )
            .route(
                "/api/labs/{lab}/containers/{container}/{action}",
                web::post().to(api::container_action),
            )
            // Snapshots (literal before the `{action}` catch-all).
            .route(
                "/api/labs/{lab}/snapshots/{name}/restore",
                web::post().to(api::snapshot_restore),
            )
            .route(
                "/api/labs/{lab}/snapshots",
                web::post().to(api::snapshot_take),
            )
            .route("/api/labs/{lab}/logs", web::get().to(logs::logs))
            // Templates (literal before the `{action}` catch-all).
            .route(
                "/api/labs/{lab}/templates",
                web::get().to(api::list_templates),
            )
            .route(
                "/api/labs/{lab}/templates/ops",
                web::get().to(api::template_ops),
            )
            .route(
                "/api/labs/{lab}/templates/{tpl}/remote",
                web::get().to(api::template_remote),
            )
            .route(
                "/api/labs/{lab}/templates/{tpl}/build",
                web::post().to(api::template_build),
            )
            .route(
                "/api/labs/{lab}/templates/{tpl}/stop",
                web::post().to(api::template_stop),
            )
            .route(
                "/api/labs/{lab}/templates/{tpl}/publish",
                web::post().to(api::template_publish),
            )
            // Playbooks: declarations, run ops, scaffolding (literal
            // before the `{action}` catch-all). Playbook files are edited
            // through the Files tab API below.
            .route(
                "/api/labs/{lab}/playbooks",
                web::get().to(playbooks::list_playbooks),
            )
            .route(
                "/api/labs/{lab}/playbooks/ops",
                web::get().to(playbooks::playbook_ops),
            )
            .route(
                "/api/labs/{lab}/playbooks/scaffold",
                web::post().to(playbooks::scaffold),
            )
            // config-weave package management over a declared playbook
            // folder (Files tab pkg buttons + repos modal).
            .route(
                "/api/labs/{lab}/playbooks/pkg",
                web::post().to(pkgs::pkg_action),
            )
            .route(
                "/api/labs/{lab}/playbooks/pkg/search",
                web::post().to(pkgs::pkg_search),
            )
            .route(
                "/api/labs/{lab}/playbooks/repos",
                web::get().to(pkgs::repos_list),
            )
            .route(
                "/api/labs/{lab}/playbooks/repos",
                web::post().to(pkgs::repos_edit),
            )
            .route(
                "/api/labs/{lab}/vms/{vm}/playbook/{action}",
                web::post().to(playbooks::run_playbook),
            )
            .route(
                "/api/labs/{lab}/containers/{container}/playbook/{action}",
                web::post().to(playbooks::run_playbook),
            )
            // Lab Files tab: the whole-lab-dir file API (literal before the
            // `{action}` catch-all).
            .route("/api/labs/{lab}/files/tree", web::get().to(files::tree))
            .route("/api/labs/{lab}/files/file", web::get().to(files::get_file))
            .route(
                "/api/labs/{lab}/files/file",
                web::put().to(files::save_file),
            )
            .route(
                "/api/labs/{lab}/files/file",
                web::delete().to(files::delete),
            )
            .route("/api/labs/{lab}/files/mkdir", web::post().to(files::mkdir))
            .route(
                "/api/labs/{lab}/files/rename",
                web::post().to(files::rename),
            )
            // Config editing (literal before the `{action}` catch-all).
            .route("/api/labs/{lab}/config", web::get().to(api::get_config))
            .route("/api/labs/{lab}/config", web::post().to(api::save_config))
            .route("/api/labs/{lab}/scripts", web::get().to(api::get_script))
            .route("/api/labs/{lab}/scripts", web::put().to(api::save_script))
            // The visual editor's structured model (literal before the
            // `{action}` catch-all).
            .route("/api/labs/{lab}/model", web::get().to(editor::get_model))
            .route(
                "/api/labs/{lab}/model/edit",
                web::post().to(editor::edit_model),
            )
            .route("/api/labs/{lab}/reload", web::post().to(api::reload_lab))
            .route("/api/labs/{lab}/{action}", web::post().to(api::lab_action))
            .route("/api/labs/{lab}", web::get().to(api::lab_status))
            // Live streams.
            .route("/api/events", web::get().to(events::events))
            .route("/api/desktop/vnc/{lab}/{vm}", web::get().to(desktop::vnc))
            .route(
                "/api/labs/{lab}/templates/{arch}/{template}/vnc",
                web::get().to(desktop::template_vnc),
            )
            // SPA + static assets.
            // The embedded documentation book (the vmlab wskill).
            .route("/help", web::get().to(help::root))
            .route("/help/{path:.*}", web::get().to(help::page))
            .default_service(web::route().to(assets::spa))
    })
    // One worker keeps the cached daemon clients on a single runtime.
    .workers(1)
    .bind((bind, port));

    let server = match server {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vmlab-web: cannot bind {bind}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match server.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmlab-web: {e}");
            ExitCode::FAILURE
        }
    }
}
