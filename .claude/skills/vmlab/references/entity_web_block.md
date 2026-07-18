# web {} block

_wcl block_

Declares an HTTP UI served inside a guest, proxied into the web console as a same-origin iframe tab — with the guest app's own login handled by the proxy.

A `web {}` block on a `vm {}` or `container {}` (which must have at least one
NIC) publishes a guest-served HTTP UI to the [web console](../references/concept_web_console.md):
the page appears as a launch card on the lab overview, opens as an in-app tab,
and all open pages aggregate under the sidebar's **Web** entry. The proxy
reaches the guest through a loopback host→guest forward, strips frame-blocking
headers, and rewrites HTML/CSS URLs and redirects so everything stays under
`/web/{lab}/{kind}/{machine}/{page}/…`.


```wcl
vm "nas" {
  template = "x86_64/linux-modern"
  nic { segment = "corp" }
  web "admin" { port = 8080  path = "/manage" }        // http://<guest>:8080/manage
}

container "grafana" {
  image = "grafana/grafana:11.2.0"
  nic { segment = "corp" }
  web "dash" {
    port = 3000
    auth { method = :form  username = "admin"  password = "admin"
           login_path = "/login"  login_body = "{\"user\":\"{user}\",\"password\":\"{pass}\"}"
           login_content_type = "application/json" }
  }
}
```

The optional `auth {}` child holds credentials the proxy injects so the guest
app's own login never prompts: `:basic`, `:bearer` (`token`), `:header`
(`header` + `value`), `:ntlm` (IIS/AD integrated; optional `domain`), or
`:form` (a login request from `login_path`/`login_body` with `{user}`/`{pass}`
substituted; captured cookies are replayed, and `fail_redirect` marks a
redirect target that means "not logged in"). Credentials are plaintext in the
lab config, like everything else there.

Console access to `/web/*` is guarded separately by a path-scoped cookie the
UI mints per session — see the [API reference](../references/fact_web_api.md). Deliberately out
of scope in v1: WebSockets/SSE and SPAs that build absolute URLs in JS; the
proxy suits admin-style UIs, not bulk downloads.


## Related

- [vm {} block](../references/entity_vms.md)

- [container {} block](../references/entity_container_block.md)

- [The web console](../references/concept_web_console.md)

- [vmlab-web](../references/entity_vmlab_web.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
