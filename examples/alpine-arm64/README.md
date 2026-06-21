# alpine-arm64

A minimal arm64 (aarch64) lab: one Alpine Linux guest on a NAT'd segment
with a host port-forward to SSH. Demonstrates running an emulated aarch64
VM under TCG on an x86 host.

## Prerequisites

Build the aarch64 Alpine template first (from the
[vmlab-templates](https://github.com/wiltaylor/vmlab) repo):

```sh
cd vmlab-templates/alpine-3.23-arm64
vmlab template build          # downloads the cloud image, slow under TCG
vmlab template list           # confirm aarch64/alpine-3.23 landed in the store
```

## Run

```sh
vmlab up                          # boots the guest (TCG — give it a minute or two)
ssh vmlab@localhost -p 12222      # password: vmlab
vmlab down
```

The provision script (`scripts/setup.wisp`) waits for the guest agent and
logs the guest's `uname -m` (should print `aarch64`) and Alpine release.
Watch it with `vmlab logs`.

> Note: on x86 hosts there is no KVM for aarch64, so the VM runs under TCG
> emulation and is slow. That's expected.
