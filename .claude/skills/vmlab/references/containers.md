# Lab containers

A lab may declare OCI containers beside its VMs. A `container` block names a
standard image (`nginx:1.27`, `ghcr.io/owner/app@sha256:…`) and carries
compose-style configuration: environment variables, volumes, port forwards, a
healthcheck, and `nic` blocks identical to a VM's. Every container runs as a
machine in a micro-VM, so it has the same segments, DNS, snapshots and agent
channel as a VM. It registers in DNS as `<name>.<lab>.<suffix>` and joins the
same `depends_on` waves.

```wcl
# examples/mixed-lab/vmlab.wcl
# A small mixed Windows/Linux lab built on the two example templates
# (examples/templates/windows-server-2025 and examples/templates/
# ubuntu-24.04 — build those first). Demonstrates a NAT'd segment, a
# static IP, boot ordering, an SMB share, a host port-forward, an OCI
# container on the same segment (§18), and a provision script driving
# both guests.
#
# vmlab up
# curl http://localhost:18080      # nginx on nix01, via the forward
# curl http://localhost:18081      # nginx in the "web" container
# vmlab down

import <vmlab.wcl>

lab "mixed-lab" {

  gui = true  # open a VNC viewer for each guest on `vmlab up`

  segment "lan" {
    subnet = "10.70.0.0/24"
    nat = true  # apt needs egress
    forward {
      host_port = 18080
      to = "nix01:80"
    }  # host → nginx
  }

  vm "winsrv" {
    template = "x86_64/windows-server-2025"
    cpus = 4
    memory = 8GiB
    nic {
      segment = "lan"
      ip = "10.70.0.10"
    }  # DHCP reservation
    share {
      host = "./shared"
      guest = "S:"
    }  # auto-mounted when ready

    # Runs once winsrv is ready; nix01 depends on it, so it waits.
    provision "scripts/setup.ws" { }
  }

  vm "nix01" {
    template = "x86_64/ubuntu-24.04"
    memory = 2GiB
    depends_on = ["winsrv"]
    nic {
      segment = "lan"
    }  # dynamic lease
  }

  # An OCI container on the same segment: pulled like a docker image,
  # run in a micro-VM, resolvable as web.mixed-lab.<suffix> from the VMs.
  container "web" {
    image = "nginx:1.27"
    # The micro-VM's size comes from the profile; declare `cpus`/`memory`
    # here to override it. One of the two must supply them.
    profile = "container"
    depends_on = ["nix01"]
    nic {
      segment = "lan"
    }
    port {
      host = 18081
      container = 80
    }  # host → container nginx
    healthcheck {
      command = ["curl", "-fsS", "http://localhost/"]
      interval = 5s
    }
  }

  on "vm.crashed" {
    run = "scripts/on-crash.ws"
  }
}
```

## The micro-VM

Each container boots a pinned Alpine `linux-virt` kernel and a purpose-built
initramfs, passed to QEMU directly with `-kernel` and `-initrd`. PID 1 in that
guest is `vmlab-cinit`, vmlab's own init. It mounts the image's root filesystem
read-only, lays a writable scratch disk over it with overlayfs, opens a control
channel to the host on the `vmlab.ctl.0` virtio-serial port, and waits for the
host to push the container's specification: the resolved command, environment,
user, working directory, mounts and NICs. It then mounts volumes, brings the
network up, spawns `vmlab-agent`, and executes the workload in its own
namespaces.

The micro-VM's `cpus` and `memory` resolve through the same chain as a VM's,
minus the template layer: the block, then its `profile`. There is no built-in
default, because what a micro-VM needs depends entirely on its image; a
container that names neither a size nor a profile supplying one is a validation
error rather than a guess that OOMs. The shipped `container` profile carries a
floor of one vCPU and 256 MiB.

Nothing here needs privileges. The host uses `/dev/kvm` when available and falls
back to TCG when it is not; no `--privileged` or added capability is ever
required.

Cross-arch containers, where the image arch differs from the host's, are out of
scope, as is a native container runtime backend. A container always runs in a
micro-VM.

## Pulling and flattening an image

An image reference resolves like a registry template reference. A first path
segment with a dot, a colon or `localhost` is a registry host; Docker Hub
shorthand normalises to `registry-1.docker.io`, so `nginx` means
`registry-1.docker.io/library/nginx`. A digest must be `@sha256:` followed by 64
hex characters. A tag is resolved against the registry to a manifest digest, a
multi-arch index is resolved to the host's platform, and every blob is
digest-verified as it streams. The supervisor pre-pulls images before the lab
daemon starts, reporting `container.pull.*` progress events.

The image's layers are then flattened into one squashfs file, with OCI whiteout
semantics applied at the tar level: a `.wh.<name>` entry deletes a lower path, an
opaque marker drops everything beneath a directory, and the highest layer wins a
path present in several. The layers are streamed rather than unpacked, first to
survey which entries survive and verify each layer's `diff_id`, then to emit the
survivors straight into `sqfstar`. The tree never touches disk, and no host
privilege is needed for ownership or device nodes.

The result is cached under `~/.local/share/vmlab/oci/` by manifest digest, with
the same lock and stage-then-rename discipline as the template store. A digest
reference, or a tag whose cached resolution is still installed, is satisfied
fully offline. When the registry is unreachable, a tag falls back to its cached
resolution with a warning.

### The pin

The digest resolved at first pull is pinned in the lab's state and never
re-pulled implicitly, so an `nginx:1.27` that moves upstream does not change
under a running lab. `vmlab container destroy`, or editing the `image =` line,
clears the pin; the next `up` resolves afresh. Every snapshot records the pin it
was taken against, because a scratch overlay means nothing without the same
read-only root, and a restore under a different pin fails, naming both digests.

## Configuration

`env` blocks pass variables to the container process. `entrypoint`, `command`,
`workdir` and `user` override the image's own, in exec form. `user` takes
`uid[:gid]` or a name the image's `/etc/passwd` knows.

A `volume` block is either a host bind (`host = "./data"`, relative to the lab
root) or a named volume (`name = "db"`), kept under the lab's `.vmlab/` and
shared by name between containers. Named volumes survive `down` and
per-container destroy; only lab `destroy` removes them. Volumes attach as
vhost-user-fs devices, one `virtiofsd` per volume, mounted natively by cinit
before the network is up. A host with no `virtiofsd` binary falls back to SMB
shares served by the lab daemon at the segment gateway, mounted over CIFS once
the network is up, exactly as shares-media.md describes for VMs. Ownership on
volume files is mount-level, not per-file container uid and gid.

A `port` block is sugar for a segment `forward` to this container. It is
installed against the container's lease when it turns ready and reinstalled
after a restart. Because volumes and ports both need the segment gateway, a
container declaring either must have at least one NIC. A container with no NIC is
otherwise valid: air-gapped, still reachable with `exec`, `cp` and `logs` over
the agent channel.

## Workload and idle mode

The default mode, `:workload`, runs the image's process, and the container's
lifecycle is that process's: when it exits, cinit reports the exit and powers
the micro-VM off. `mode = :idle` boots the micro-VM, mounts everything and
starts the agent, but never runs the entrypoint. The container then stays up for
`exec`, `shell` and an SSH attach until you stop it. That is what a dev
container wants, since it has no service to be.

## Readiness and health

Readiness is two-stage. The first stage is cinit reporting `started`, or `idle`
in idle mode. The second, when the block declares a `healthcheck`, is the first
passing probe: the command runs inside the container at the declared interval,
after the start period, and a consecutive-failure count marks the container
`unhealthy`. In idle mode there is no workload to prove liveness, so the second
stage waits for the agent instead.

Readiness is deliberately not gated on the agent in workload mode. The
entrypoint runs regardless, as it would under Docker, and an agent hiccup must
not wedge a `depends_on` wave. The agent is polled separately and gates only
`exec` and `cp`. The events `container.starting`, `ready`, `stopped`, `crashed`
and `unhealthy` are bindable with `on`, exactly like their VM counterparts, and
readiness gates the dependents of a container the same way a VM's does.

## Stopping, logs and snapshots

The stop ladder mirrors a VM's: a stop signal to the process with a grace
period, then a guest shutdown, then a kill. The container's stdout and stderr are
the micro-VM's serial console, so `vmlab container logs` shows the kernel's boot
messages and then the process's output, and `-f` follows it.

Containers snapshot with full VM parity. An offline snapshot captures the scratch
disk; an online one captures scratch, RAM and device state, both as
qcow2-internal snapshots of the per-container scratch disk, with the immutable
root outside the snapshot. Restoring an online snapshot resumes the process
mid-flight. Volume contents are host state, outside snapshot scope.

## The container identity floor

Every session into a container lands as some account. When the container declares
no `login`, that account is the container floor: the user cinit resolved for the
workload, which is the declared `user`, else the image's `USER`, else root. This
is devcontainers' `remoteUser` idea, and it costs nothing because Linux needs no
credential to become that user. A `login` block on a container may therefore
declare the account alone, with no password. See logins-and-ssh.md for the rest
of the identity ladder.

## container {}

An OCI container run inside a micro-VM. It also takes `login {}`, `nic {}`,
`provision {}` and `playbook {}`, which mean the same thing as on a VM
(vm.md), and may carry the `@dev` decorator described there.

```wcl
container "<name>" {
  image      = "nginx:1.27"
  mode       = :workload
  entrypoint = ["/bin/sh", "-c"]
  command    = ["…"]
  workdir    = "/app"
  user       = "1000:1000"
  profile    = "container"
  cpus       = 1
  memory     = 256MiB
  depends_on = ["db"]
  nic         { … }
  env         { … }
  volume      { … }
  port        { … }
  healthcheck { … }
  login "…"   { … }
  provision "…" { … }
  playbook "…"  { … }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Container name, a DNS label, unique per lab. VMs and containers share one namespace; the inline block label. |
| `image` | utf8 | required | OCI image reference, for example `nginx:1.27` or `ghcr.io/owner/app@sha256:…`. |
| `mode` | symbol | `:workload` | `:workload` starts the OCI process; `:idle` keeps the micro-VM available for exec without running it. |
| `entrypoint` | list<utf8> | image default | Override the image entrypoint, in exec form. |
| `command` | list<utf8> | image default | Override the image cmd, in exec form. |
| `workdir` | utf8 | image default | Working directory inside the container. |
| `user` | utf8 | image default | User to run as: `uid[:gid]` or a name from the image. |
| `profile` | utf8 | none | Guest profile supplying micro-VM hardware defaults, for example `container`. |
| `cpus` | i64 | from profile | vCPU count for the micro-VM, greater than 0. One of this field or the profile must supply it. |
| `memory` | ByteSize | from profile | RAM for the micro-VM, for example `512MiB`. One of this field or the profile must supply it. |
| `depends_on` | list<utf8> | none | VM or container names to wait for before this one. No cycles. |
| `nic {}` | children | none | Network interfaces. None means air-gapped; exec and copy still work via the agent. |
| `env {}` | children | none | Environment variables passed to the container process. |
| `volume {}` | children | none | Host binds and named volumes mounted into the container. |
| `port {}` | children | none | Host-to-container port forwards; sugar for a segment `forward` to this container. |
| `healthcheck {}` | child | none | Health probe gating readiness. Without one the container is ready once its process starts. |
| `login {}` | children | none | Identities a surface attaches to this container as. Without one it falls to the user cinit resolves. |
| `provision {}` | children | none | wscript scripts run on `vmlab up` once this container is ready, interleaved with its playbooks in declaration order. |
| `playbook {}` | children | none | config-weave playbooks applied on `vmlab up`, interleaved with its provisions in declaration order. |

Validation enforces these rules:

- The name is a DNS label and no VM or container in the lab has it.
- `image` is non-empty, has no whitespace, and any digest is well formed.
- `profile`, if set, names a known profile. `cpus` and `memory` both resolve
  through the container block, then the profile; a container that neither
  declares a size nor names a profile supplying one is an error rather than a
  guess.
- An `:idle` container cannot declare `entrypoint`, `command` or a
  `healthcheck`.
- A container with `port {}` blocks needs at least one NIC; forwards need a
  segment to reach it over.
- A container with `volume {}` blocks needs at least one NIC; volumes mount over
  the network from the segment gateway.
- Every name in `depends_on` exists, with no cycle across VMs and containers.
- `login {}` blocks are judged against the Linux family regardless of the
  profile named: `elevated` is rejected and `password` is optional.

```wcl
# examples/mixed-lab/vmlab.wcl
container "web" {
  image = "nginx:1.27"
  # The micro-VM's size comes from the profile; declare `cpus`/`memory`
  # here to override it. One of the two must supply them.
  profile = "container"
  depends_on = ["nix01"]
  nic {
    segment = "lan"
  }
  port {
    host = 18081
    container = 80
  }  # host → container nginx
  healthcheck {
    command = ["curl", "-fsS", "http://localhost/"]
    interval = 5s
  }
}
```

A dev container uses `:idle` mode, since it has no service to be, and a `login`
for the account an editor attaches as.

```wcl
# examples/dev-neovim-container/vmlab.wcl
@dev(default = true, workspace = "./workspace")
container "dev01" {
  image   = "alpine:3.22"
  profile = "container"
  // Neovim wants more than the profile's floor, and a container names its
  // own size when the profile's is not the right one.
  cpus    = 2
  memory  = 1GiB
  // `:idle` keeps the micro-VM up for attaching without running the
  // image's entrypoint — a dev container has no service to be.
  mode    = :idle
  nic { segment = "lan" }

  // The container identity floor (§19.2): the agent is root and root needs
  // no credential to become an account, so a Linux `login {}` may declare
  // the account alone. Its Windows twin cannot — every credential-free
  // route there is the one Windows OpenSSH's S4U logon already
  // disqualified, and `elevated` is a validation error on this side.
  login "dev" { user = "dev" default = true }

  provision "scripts/dev-user.ws" { }
  provision "scripts/editor-bits.ws" { }
}
```

## env {}

One environment variable passed to the container process.

```wcl
env { name = "NGINX_PORT" value = "8080" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 | required | Variable name. |
| `value` | utf8 | required | Variable value. |

A name that is empty or contains `=` is rejected.

## volume {}

A mount into the container: a host directory bound by path, or a named volume
kept under the lab directory. Exactly one of `host` and `name` is set.

```wcl
volume { host = "./site" target = "/usr/share/nginx/html" read_only = true }
volume { name = "pgdata" target = "/var/lib/postgresql/data" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | utf8 | none | Host path to bind-mount, relative to the lab root. One of `host` or `name` is required. |
| `name` | utf8 | none | Named volume kept under the lab dir, shared by name, retained until lab destroy. One of `host` or `name`. |
| `target` | utf8 | required | Absolute mount path inside the container. |
| `read_only` | bool | `false` | Mount read-only. |

Validation rejects a volume with both `host` and `name`, or neither. A `host`
path must be a directory under the lab root. A `name` becomes a directory name,
so it cannot be empty, `.`, `..`, or contain a slash. `target` must start with
`/`. Named volumes are lab-scoped: they survive `down` and a per-container
destroy, and only lab `destroy` removes them. Volume contents are outside
snapshot scope.

## port {}

A host-to-container port forward. It compiles into the same forward machinery as
a segment `forward {}` and is installed against the container's lease when it
becomes ready.

```wcl
port { host = 18081 container = 80 proto = "tcp" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `host` | i64 | required | Host port to listen on, 1 to 65535. Unique across the lab. |
| `container` | i64 | required | Container port to forward to, 1 to 65535. |
| `proto` | utf8 | `tcp` | Protocol: `tcp`, `udp` or `both`. |

The host port must be unused by every other `port {}` and every segment
`forward {}` in the lab, and the container needs a NIC.

## healthcheck {}

A probe run inside the container. Readiness is two-stage: the process starts,
then the first probe passes. Only then does the container count as ready for
`depends_on` waves. Consecutive failures past `retries` fire
`container.unhealthy`.

```wcl
healthcheck {
  command      = ["curl", "-fsS", "http://localhost/"]
  interval     = 10s
  timeout      = 5s
  retries      = 3
  start_period = 10s
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | list<utf8> | required | Probe command run inside the container, in exec form. Exit 0 means healthy. |
| `interval` | Duration | `10s` | Time between probes. |
| `timeout` | Duration | `5s` | Per-probe timeout. |
| `retries` | i64 | `3` | Consecutive failures before unhealthy. |
| `start_period` | Duration | `10s` | Grace period after start before failures count. |

Validation requires a non-empty `command`, `interval` and `timeout` greater than
zero, `retries` at least 1, and a non-negative `start_period`. An `:idle`
container cannot declare one.
