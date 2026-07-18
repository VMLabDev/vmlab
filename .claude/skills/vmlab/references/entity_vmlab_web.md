# vmlab-web

_tool_

The web UI server: an Actix-web binary exposing vmlab over REST + WebSockets and serving the embedded console UI, with username/password auth.

`vmlab-web` serves the [web console](../references/concept_web_console.md) and its
[REST + WebSocket API](../references/fact_web_api.md). It talks to the same supervisor and
lab daemons the CLI does, over the existing unix-socket protocol — no daemon
changes, no extra privileges. The official container image runs it as the
default command; on a host it is just a second binary next to `vmlab`
(built with the optional `web` cargo feature).


```console
vmlab-web                                   # local-only: http://127.0.0.1:7878, no login
vmlab-web --bind 0.0.0.0 \
  --user admin --password-hash '$argon2id$…'   # exposed: login required
```

| Flag | Env fallback | Meaning |
| --- | --- | --- |
| `--bind <ip>` | — | Address to bind (default `127.0.0.1`; non-loopback implies `--auth`) |
| `--port <n>` | — | TCP port (default `7878`) |
| `--auth` | — | Require login (auto-enabled for non-loopback binds); errors without credentials |
| `--no-auth` | — | Explicitly allow a non-loopback bind with no login (ignored once credentials are set) |
| `--user <name>` | `VMLAB_WEB_USER` | Login username |
| `--password <pw>` | `VMLAB_WEB_PASSWORD` | Login password, hashed once at startup (prefer a hash) |
| `--password-hash <phc>` | `VMLAB_WEB_PASSWORD_HASH` | Pre-computed argon2 PHC hash — wins over `--password` |
| `--up` | `VMLAB_WEB_UP` | Bring the working-directory lab up in the background on startup |
| `--trust-proxy` | `VMLAB_WEB_TRUST_PROXY` | Behind a reverse proxy: rate-limit logins by the proxy-appended `X-Forwarded-For` entry |
| — | `VMLAB_WEB_SESSION_TTL_SECS` | Idle session lifetime (default 12 hours) |

**Secure default:** a username plus a password/hash enables auth regardless of
other flags; with no credentials, running open is allowed only on a loopback
bind or with an explicit `--no-auth` opt-in — a bare non-loopback bind is
refused at startup. Sessions ride an `Authorization: Bearer` token issued by
`POST /api/login`; the auth gate covers everything under `/api/` (the guest
web-page proxy under `/web/` uses its own path-scoped cookie — see the
[web {} block](../references/entity_web_block.md)). The server also embeds this reference
book at `/help`, which the console's Help button opens as an in-app tab.


## Related

- [The web console](../references/concept_web_console.md)

- [vmlab-web: the REST + WebSocket API](../references/fact_web_api.md)

- [Serve the web console](../references/process_serve_web_console.md)

- [Daemon model](../references/concept_daemon_model.md)

- [Containers](../references/concept_containers.md)

[← Back to SKILL.md](../SKILL.md)
