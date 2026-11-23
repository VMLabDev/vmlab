# mixed-lab — Windows Server 2025 + Ubuntu 24.04

A two-VM lab on one NAT'd segment exercising the example templates plus a
handful of vmlab features: static IP reservation, `depends_on` ordering,
an SMB `share` onto a Windows drive letter, a `forward` from the host, a
provision script, and a crash handler.

Prerequisites — build both templates first:

```sh
(cd ../templates/ubuntu-24.04            && vmlab template build)
(cd ../templates/windows-server-2025     && ./fetch-deps.sh && vmlab template build)
```

Run it:

```sh
vmlab validate
vmlab up                        # boots winsrv, then nix01; runs setup.ws
vmlab status                    # both ready: winsrv 10.70.0.10, nix01 leased
curl http://localhost:18080      # nginx on nix01 through the segment forward
vmlab down                      # clones retained; `vmlab destroy` deletes them
```

Guest credentials: `Administrator` / `vmlab123!` (Windows), `vmlab` /
`vmlab` (Ubuntu). The `shared/` folder appears on winsrv as `S:\`.

Note on the share and desktop sessions: the daemon maps `S:` as SYSTEM,
which makes the letter visible everywhere but authenticated only for
SYSTEM (what provision scripts and `vmlab exec` use). An interactive
user opening `S:` sees "user name or password is incorrect" until their
session has the lab credential — double-click **vmlab-shares** on the
guest's desktop (vmlab drops it there) once per user; it stores the
credential and `S:` opens normally from then on. Credentials persist in
`.vmlab/smb/machines/<machine>/creds` across `vmlab up` cycles; `vmlab destroy` rotates
them (re-run the desktop script after a destroy).
