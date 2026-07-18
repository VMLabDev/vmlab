# container {} block

_wcl block_

Declares an OCI container in a lab: image reference, env/volumes/ports/healthcheck, restart policy — run in a micro-VM on the same segments as VMs.

A `container {}` block inside a `lab {}` runs a standard OCI container image
(docker-style: `nginx:1.27`, `ghcr.io/owner/app@sha256:…`; Docker Hub shorthand
is normalised) alongside the lab's VMs. Under the hood the container boots in a
tiny micro-VM (pinned Alpine kernel + purpose-built init), so it works in the
official vmlab container image with no extra privileges — but from the lab's
point of view it is just another machine (PRD §18).


```wcl
container "web" {
  image      = "nginx:1.27"
  depends_on = ["db"]                    // VM or container names — one namespace
  restart    = "on-failure"              // "no" (default) | "on-failure" | "always"
  nic    { segment = "corp" ip = "10.50.0.20" }
  env    { name = "MODE" value = "prod" }
  volume { name = "data" target = "/var/lib/data" }        // named, lab-scoped
  volume { host = "./www" target = "/srv/www" read_only = true }
  port   { host = 18080 container = 80 }                   // host → container
  healthcheck { command = ["curl", "-fsS", "http://localhost/"] interval = 5s }
}
```

```wcl
container "toolbox" {
  image = "ubuntu:24.04"
  mode  = :idle                          // do not run the image entrypoint/cmd
}
```

VM and container names share one namespace: DNS registration
(`web.<lab>.<suffix>` and plain `web`), `depends_on` waves, segment
`forward { to = "web:80" }` targets and provision scoping resolve across both
kinds. NICs are optional — a container with none is air-gapped but still
reachable with `vmlab container exec` / `cp` over the agent channel. The image
digest resolved at first pull is pinned in lab state (never re-pulled
implicitly); `vmlab container destroy` or editing `image =` clears the pin.
Readiness gates on the process starting plus the first passing healthcheck;
`restart` respawns with backoff on exit. Containers snapshot with \*\*full VM
parity\*\* — offline and online, standalone or as part of a lab-wide
`vmlab snapshot` — and volumes ride virtiofs (CIFS fallback), like
[shares](../references/entity_shares.md). Named volumes survive `down` and per-container
destroy; lab `destroy` removes them.

With `mode = :idle`, the root filesystem, volumes, networking and guest agent
come up without starting the image entrypoint or command. Readiness gates on
the agent instead, image healthchecks do not run, and commands can be launched
with `vmlab container exec` or from provision scripts.


## Related

- [lab {} block](../references/entity_labs.md)

- [vm {} block](../references/entity_vms.md)

- [nic {} block](../references/entity_nic_block.md)

- [segment {} block](../references/entity_segment_block.md)

- [Container](../references/entity_container_api.md)

[← Back to SKILL.md](../SKILL.md)
