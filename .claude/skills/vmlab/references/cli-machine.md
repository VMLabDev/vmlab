# vmlab CLI: machine-level verbs

Per-machine commands: `vm`, `machine`, `container`, `exec`, `shell`, `console`, `cp`, `clipboard`, `snapshot`, `dev`, `ssh`, `ssh-config`. Lab-level verbs (`up`, `down`, `destroy`, `status`, `validate`, `lab`) are in cli-lab.md.

Machine references take the form `[lab/]name`. A bare name resolves against the lab in the current directory and starts the lab daemon if none is running. The qualified form addresses a lab already running, from any directory, and never starts one; it fails with `lab "<name>" is not running` otherwise. A malformed reference such as `lab/` is refused before anything is contacted. Exceptions are noted per verb (`console`, `ssh`, `dev sync` never start a daemon).

## vmlab vm

Controls one VM at a time: power, IP address, and display-driven interaction (screenshot, input, OCR, template matching — see snapshots-vision.md; the wscript Machine handle offers the same surface to scripts, see wscript-machine-api.md).

```sh
vmlab vm <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `start` | Start one VM. |
| `ip` | Print a VM's IP address (defaults to the first NIC with a lease). |
| `stop` | Stop one VM (graceful ladder; `--force` to kill). |
| `restart` | Restart one VM. |
| `destroy` | Destroy one VM: stop it and delete its clone (config retained). |
| `screenshot` | Capture a running VM's screen to a PNG file. |
| `sendkeys` | Send a key chord (e.g. `ctrl-alt-delete`). |
| `mouse-move` | Move the mouse pointer to absolute screen coordinates. |
| `click` | Click a mouse button, optionally first moving to x,y. |
| `drag` | Press, drag from x1,y1 to x2,y2, and release the left button. |
| `ocr` | OCR the screen (optionally a region). |
| `find-image` | Search the screen for a template image. |
| `-h`, `--help` | Print help. |

The power subcommands and `ip` accept a container name too and behave the same; `vmlab container` is the same request under the other noun. The display subcommands need a framebuffer, which a container never has, so on a container they fail with `unsupported`.

### vmlab vm start

```sh
vmlab vm start <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Starts one VM without running the lab's `up` plan: no dependency waves, no provision scripts, no playbooks. Any deferred download the VM needs runs first with progress on the terminal, then the host binaries are checked, then the VM boots. A VM whose clone does not exist yet gets one created from its template, and its first-boot script runs before it reports ready, exactly as under `vmlab up`. A VM that is already running is left alone. Nothing is printed on success.

### vmlab vm ip

```sh
vmlab vm ip [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `--nic <NIC>` | Report this NIC's address instead of the first one. |
| `-h`, `--help` | Print help. |

Asks the guest agent for its interfaces, matches them to the VM's NICs by MAC address and prints one IPv4 address followed by a newline. With no `--nic` it is the first NIC that holds an address, in declaration order. `--nic` takes a zero-based index into the VM's `nic {}` blocks and reports that NIC's address, failing with `no IPv4 address reported by agent` if that NIC has none. The VM must be running with its agent answering; a VM still booting fails the same way.

### vmlab vm stop

```sh
vmlab vm stop [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `--force` | Hard kill instead of the graceful ladder. |
| `-h`, `--help` | Print help. |

Stops one VM through the graceful ladder: a shutdown through the guest agent, then an ACPI power-down, then a hard kill, each with a timeout (the rungs and timeouts are `vmlab down`'s, in cli-lab.md). `--force` kills QEMU at once. Unlike `down`, this stops only the named VM; machines that depend on it keep running. A VM that is already stopped is a no-op. Nothing is printed on success.

### vmlab vm restart

```sh
vmlab vm restart <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Stops the VM through the graceful ladder, waits up to 60 seconds for it to settle as stopped, and boots it again. The clone is kept, so the guest comes back with its disk state and no first-boot script. Port forwards that target the VM are re-installed by the daemon when it gets a new lease. A VM that does not settle in time fails with `<name> did not stop for restart`.

### vmlab vm destroy

```sh
vmlab vm destroy <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

The per-machine form of `vmlab destroy`. The VM's SSH multiplexer is told to exit while its alias still resolves, the workspace syncer for it is stopped, the VM is force-stopped and its clone, runtime directory, snapshots, workspace sync ledger and any agent-repair divergence are deleted. The VM stays declared in the lab file, so a later `vmlab up <vm>` re-creates it from the template and runs its first-boot script again. The rest of the lab keeps running. Prints `vm "<name>" destroyed`.

Warning — snapshots go with the clone: a VM's snapshots live inside its qcow2 clone. `vmlab vm destroy` deletes them along with the disk, and nothing restores them.

### vmlab vm screenshot

```sh
vmlab vm screenshot <VM> <PATH>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `<PATH>` | Output PNG path. |
| `-h`, `--help` | Print help. |

Captures the VM's framebuffer to a PNG. A relative path is made absolute against your current directory before it is sent, because the daemon writes the file and has a working directory of its own. The path written is printed on success. The VM must be running.

### vmlab vm sendkeys

```sh
vmlab vm sendkeys <VM> <CHORD>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `<CHORD>` | The key chord, e.g. `ctrl-alt-delete`. |
| `-h`, `--help` | Print help. |

Presses and releases one chord on the VM's virtual keyboard, in the same spelling the wscript `send_keys` call accepts: key names joined with `-`. To type a string rather than a chord use a script and the `type` call (see snapshots-vision.md). Nothing is printed on success.

### vmlab vm mouse-move

```sh
vmlab vm mouse-move <VM> <X> <Y>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `<X>`, `<Y>` | Absolute screen coordinates, in pixels from the top-left corner. |
| `-h`, `--help` | Print help. |

Moves the pointer to an absolute position on the guest screen without clicking. Nothing is printed on success.

### vmlab vm click

```sh
vmlab vm click [OPTIONS] <VM> [X] [Y]
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `[X]`, `[Y]` | Move here before clicking. Omit both to click at the current position. |
| `--button <BUTTON>` | Button to click: `left`, `right` or `middle`. Default `left`. |
| `-h`, `--help` | Print help. |

Clicks one button, moving the pointer first when coordinates are given. Giving only one of `X` and `Y` is refused with `click coordinates need both x and y` before anything is contacted. Nothing is printed on success.

### vmlab vm drag

```sh
vmlab vm drag <VM> <X1> <Y1> <X2> <Y2>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `<X1>`, `<Y1>` | Where the left button is pressed. |
| `<X2>`, `<Y2>` | Where it is released. |
| `-h`, `--help` | Print help. |

Presses the left button at the first point, moves to the second and releases. Nothing is printed on success.

### vmlab vm ocr

```sh
vmlab vm ocr [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `--region <X> <Y> <W> <H>` | Restrict to a region: x y w h. |
| `-h`, `--help` | Print help. |

Runs OCR over the current screen, or over the rectangle `--region` names, and prints the recognised text followed by a newline. The four region values are separate arguments; negative values are clamped to 0. The recogniser and its limits are in snapshots-vision.md.

### vmlab vm find-image

```sh
vmlab vm find-image [OPTIONS] <VM> <IMAGE>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. |
| `<IMAGE>` | Template image path (PNG/PPM). |
| `--threshold <THRESHOLD>` | Match threshold 0.0–1.0. Default 0.9. |
| `--region <X> <Y> <W> <H>` | Restrict the search to a region: x y w h. |
| `-h`, `--help` | Print help. |

Searches the screen for the template image and prints the best match as one line, `x=.. y=.. w=.. h=.. score=.. center=x,y`. The image path is resolved on the host before the request is sent, so a missing file is refused with `reference image <path>` and no daemon is contacted. When nothing scores at or above the threshold the verb prints `no match` on stderr and exits 1, which makes it usable as a condition in a shell loop.

### Examples

Restart a VM and wait for it to get an address:

```sh
vmlab vm restart client01
until vmlab vm ip client01 2>/dev/null; do sleep 5; done
```

Log in on a Windows console from the shell:

```sh
vmlab vm sendkeys client01 ctrl-alt-delete
vmlab vm screenshot client01 /tmp/login.png
vmlab vm ocr client01 --region 0 400 1024 200
```

Wait for a button to appear, then click it:

```sh
until out=$(vmlab vm find-image client01 ./ok-button.png); do sleep 2; done
center=${out##*center=}
vmlab vm click client01 ${center%,*} ${center#*,}
```

### Exit status

0 on success. Exit 4 (`not_found`) means the lab declares no VM by that name, for `stop`, `ip` and every display subcommand. Exit 6 (`unsupported`) means the machine has no display, which is every container. `start`, `restart` and `destroy` exit 1 (`failed`) on an unknown name and on any boot, stop or disk failure; the other subcommands exit 1 on a VM that is not running, an agent that does not answer, or a local problem such as an unreadable image or an unreachable lab. `find-image` exits 1 on no match. Exit 5 (`conflict`) means the supervisor tracks a lab with this name from another directory. A usage error, including a bad `--button` value, exits 2.

## vmlab machine

Asks a machine, VM or container, what it can do and how it is doing, and holds the one verb that changes a running machine's guest agent in place. Both kinds answer the same commands.

```sh
vmlab machine <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `capabilities` | What a machine can do beyond the universal commands, probed live: a display, a console log, in-place reboot, a healthcheck, and whichever features its agent negotiated. |
| `stats` | Latest guest metrics: CPU, memory and mounted filesystems. |
| `repair-agent` | Push the agent this vmlab ships into a running machine, and mark that machine diverged. |
| `-h`, `--help` | Print help. |

Each subcommand renders a report for a person by default and prints the daemon's payload as pretty JSON under `--json`.

### vmlab machine capabilities

```sh
vmlab machine capabilities [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `--json` | Emit the raw JSON instead of a report. |
| `-h`, `--help` | Print help. |

Reports what this machine can serve, probed live rather than inferred from its kind, one line per capability:

| Line | Meaning |
| --- | --- |
| `kind` | `vm` or `container`. |
| `display` | `yes` when the machine has a framebuffer, so the display subcommands of `vmlab vm` and the console work. |
| `console log` | `yes` when a console log is readable from the host, as `vmlab container logs` reads it. |
| `reboot` | `yes` when the guest can reboot in place and come back. |
| `healthcheck` | `yes` when the machine declares a healthcheck, so its status carries a verdict. |
| `attachable` | `yes` when the agent negotiated both `tunnel` and `fileops`, the two features an attach over the SSH facade needs. |
| `agent` | The features the agent negotiated at handshake, comma-separated, or `-` when no agent is answering. |

Agent features come from a live handshake, so a machine that is up but not yet answering reports `-`, which reads differently from a feature list that lacks something. The possible features are `terminal`, `exec`, `fileops`, `tail`, `metrics`, `clipboard`, `eventlog`, `tunnel` and `watch`. `attachable` says whether this agent can serve an attach at all, never whether your attach will succeed: identity is declared separately. The failure ladder built on this flag is in logins-and-ssh.md.

### vmlab machine stats

```sh
vmlab machine stats [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `--json` | Emit the raw JSON instead of a report. |
| `-h`, `--help` | Print help. |

Prints the guest's latest metrics: a `cpu` line as a percentage, a `memory` line as `used / total (pct%)`, and one `disk <mount>` line per mounted filesystem in the same form. Sizes are rendered in binary units; a filesystem whose total the guest has not reported shows only the used figure. The agent is given 10 seconds to answer.

Reading is not free of side effects. It subscribes the daemon's sampler, so a machine nothing had asked about starts being sampled every two seconds from the first read. The JSON form carries `cpu_pct`, `mem_used`, `mem_total` and a `disks` array of `mount`, `used` and `total`, all in bytes.

### vmlab machine repair-agent

```sh
vmlab machine repair-agent [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `--json` | Emit the raw JSON instead of a report. |
| `-h`, `--help` | Print help. |

Pushes the agent binary this `vmlab` ships into a running machine over the machine's own agent channel, restarts the agent, and records the machine as **diverged**. The agent normally enters a machine once, when its template is built, and the template's sealed `agent_version` describes every clone of it. After a repair that is no longer true for this clone, so `vmlab status` shows `diverged=yes` under `-v` until the disks are destroyed. Nothing runs this by itself; it is a tool, not a policy. Rebuilding the template is the other remedy and the one that keeps "same template, same machine" true.

The report says what landed and what the machine now is: `pushed <version> to <path> on "<machine>"`, then `agent <version> answering, attachable yes|no`, then `"<machine>" is now diverged from its template — vmlab vm destroy + vmlab up puts it back on the sealed agent`. The JSON form carries `machine`, `pushed`, `installed_at`, `agent_version`, `features` and `attachable`.

The machine must be running with its agent answering, and that agent must serve `fileops`, because the binary rides the agent's own file vocabulary. An agent too old to serve it cannot be replaced this way at all; the verb refuses and names the rebuild as the only remedy.

Meaningless on a container: a lab container's agent lives in the initramfs guest asset this host installed, not in anything the container boots, so it already tracks the vmlab you are running and cannot go stale. The verb refuses and says so instead of pushing anything. Refreshing it means reinstalling the guest asset.

### Examples

Check why an editor cannot attach to a dev machine:

```sh
vmlab machine capabilities dev01
```

```sh
kind         vm
display      yes
console log  yes
reboot       yes
healthcheck  no
attachable   no
agent        terminal, exec, tail, metrics, clipboard
```

Watch memory on a build box:

```sh
watch -n 2 vmlab machine stats buildbox
```

Bring a stale agent up to date without rebuilding the template:

```sh
vmlab machine repair-agent dev01
```

### Exit status

0 on success. Exit 4 (`not_found`) means the lab declares no machine by that name. A machine that is not running, an agent that does not answer or lacks the `metrics` feature, a push that fails, and a repair attempted on a container all exit 1 (`failed`). Exit 5 (`conflict`) means the supervisor tracks a lab with this name from another directory. A usage error exits 2.

## vmlab container

Controls one lab container at a time: its lifecycle, a command or a shell inside it, its console log and its IP address. A lab container runs as a micro-VM, so the power subcommands are the same requests `vmlab vm` makes under the other noun. See containers.md.

```sh
vmlab container <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `start` | Start one container. |
| `stop` | Stop one container (graceful ladder; `--force` to kill). |
| `restart` | Restart one container. |
| `destroy` | Destroy one container: stop it and delete its scratch state (config retained). |
| `exec` | Run a command inside the container via the agent. |
| `logs` | Tail or dump a container's console log (kernel + stdout/stderr). |
| `ip` | Print a container's IP address. |
| `shell` | Attach an interactive shell inside the container (Ctrl-] to detach). |
| `-h`, `--help` | Print help. |

### vmlab container start

```sh
vmlab container start <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Starts one container without running the lab's `up` plan. The image is pulled first if it is still pending, with progress on the terminal, then the host binaries are checked and the micro-VM boots. When the container declares volumes the bundled SMB server is started so they can mount. A container that is already running is left alone. Nothing is printed on success.

### vmlab container stop

```sh
vmlab container stop [OPTIONS] <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `--force` | Hard kill instead of the graceful ladder. |
| `-h`, `--help` | Print help. |

Stops one container through its ladder. The first rung asks the container's init over the control channel to signal the entrypoint and power off once it exits, waiting the image's stop grace (default 10 seconds) plus 15 seconds. The second rung is a shutdown through the guest agent, and the last a hard kill. `--force` kills at once. Only the named container stops; its dependents keep running. Nothing is printed on success.

### vmlab container restart

```sh
vmlab container restart <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Stops the container gracefully, waits up to 60 seconds for it to settle, and boots it again. Its scratch overlay is kept, so files written since the last `destroy` survive. Nothing is printed on success.

### vmlab container destroy

```sh
vmlab container destroy <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Force-stops the container and deletes its scratch overlay, runtime directory and snapshots. Named volumes are lab-scoped and survive until `vmlab destroy`. The container stays declared, so a later `vmlab up <container>` re-creates it from the image. Prints `container "<name>" destroyed`.

### vmlab container exec

```sh
vmlab container exec [OPTIONS] <CONTAINER> [-- <CMD>...]
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `[CMD]...` | Command and arguments, after `--`. |
| `--timeout <SECS>` | Seconds to wait for the command to finish. Default 120. |
| `--user <LOGIN>` | Run as this login: the label a `login {}` block declares, or the account name as an alias. Defaults to the machine's default login; `SYSTEM` (Windows) or `root` (Linux) is the agent identity. |
| `--password <PASSWORD>` | Password for an account the lab file does not declare, or one whose declared password has been rotated. Requires `--user`. |
| `-h`, `--help` | Print help. |

Runs one command inside the container through the guest agent, the container spelling of `vmlab exec`. The command's stdout and stderr are mirrored to this process's stdout and stderr once it finishes, and its exit code becomes this verb's exit code. Without a command the verb refuses with `nothing to execute — usage: vmlab container exec <container> -- <cmd> [args...]`.

Who the command runs as follows the login ladder in logins-and-ssh.md: `--user` first, then the container's default `login {}`, then the agent identity, `root`. A container has no PAM, so a declared login is entered by `setuid`, the container identity floor. A login that cannot be resolved fails naming both the account and the machine.

### vmlab container logs

```sh
vmlab container logs [OPTIONS] <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `-f`, `--follow` | Keep following. |
| `-n`, `--lines <LINES>` | Lines of history to show. Default 100. |
| `-h`, `--help` | Print help. |

Prints the tail of the container's console log, which carries the micro-VM's kernel messages and the entrypoint's stdout and stderr. With `--follow` the tail is printed and then the daemon polls the log every half second and streams what was appended, until you press Ctrl-C or the container stops. An empty log prints nothing. The log is read from the host, so it works while the container is stopped and needs no agent.

### vmlab container ip

```sh
vmlab container ip <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Prints the first IPv4 address the guest agent reports on one of the container's NICs. The container must be running with its agent answering.

### vmlab container shell

```sh
vmlab container shell [OPTIONS] <CONTAINER>
```

| Option | Meaning |
| --- | --- |
| `<CONTAINER>` | The container, as `[lab/]name`. |
| `--user <LOGIN>` | Run as this login: the label a `login {}` block declares, or the account name as an alias. Defaults to the machine's default login; `root` is the agent identity. |
| `--password <PASSWORD>` | Password for an account the lab file does not declare, or one whose declared password has been rotated. Requires `--user`. |
| `-h`, `--help` | Print help. |

Opens an interactive terminal inside the container over the agent's virtio-serial channel, so it works with no guest network. Every attach opens a fresh session at your terminal's current size, and resizes follow the local window. The local terminal goes raw; `Ctrl-]` detaches, as in `telnet`. The banner `connected to "<name>" — escape character is ^]` is printed on connect. Identity is resolved the same way as for `exec`. The same shell is reachable through `ssh` once the lab's managed block is in place.

### Examples

Run a health probe inside the mixed-lab web container:

```sh
vmlab container exec web -- wget -qO- http://localhost/
```

Watch the entrypoint start up:

```sh
vmlab container logs -f -n 20 web
```

Open a shell as the dev login on the neovim example's container:

```sh
vmlab container shell dev-neovim-container/dev01 --user dev
```

### Exit status

0 on success. `exec` exits with the guest command's own exit code when it is non-zero. Exit 4 (`not_found`) means the lab declares no container by that name, for `stop`, `exec`, `logs`, `ip` and `shell`. Exit 6 (`unsupported`) is what `logs` answers on a machine with no console log. `start`, `restart` and `destroy` exit 1 (`failed`) on an unknown name and on any boot, stop or disk failure; every subcommand exits 1 on a container that is not running, an agent that does not answer, a login that cannot be resolved, or a lab that cannot be reached. Exit 5 (`conflict`) means the supervisor tracks a lab with this name from another directory. A usage error, including `--password` without `--user`, exits 2.

## vmlab exec

Runs one command inside a guest through the vmlab agent and mirrors its standard output, standard error, and exit code. It rides the agent's virtio-serial channel, so it works on a machine with no guest network. A VM and a container answer the same request; `vmlab container exec` is the same verb under the container noun.

```sh
vmlab exec [OPTIONS] <VM> [-- <CMD>...]
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `[CMD]...` | Command and arguments, after `--`. |
| `--timeout <SECS>` | Seconds to wait for the command to finish. Default: 120. |
| `--user <LOGIN>` | Run as this login: the label a `login {}` block declares, or the account name as an alias. Default: the machine's default login. `SYSTEM` on Windows or `root` on Linux is the agent identity. |
| `--password <PASSWORD>` | Password for an account the lab file does not declare, or one whose declared password has been rotated. Requires `--user`. |
| `-h`, `--help` | Print help. |

The first word after `--` is the program and the rest are its arguments; nothing is passed through a shell, so quote and glob on the host side or invoke a shell explicitly. The command is refused locally when nothing follows `--`. Standard output goes to the terminal's standard output and standard error to its standard error, so the two can be redirected separately.

Which account the command runs as follows the login ladder in logins-and-ssh.md. On a machine that declares a `login {}`, the command runs as that login rather than as the agent, so writing into a system directory starts failing where it used to work. `--user SYSTEM` or `--user root` is the old behaviour, spelled out. `--user` also accepts an account the lab file never declared when `--password` is given. A logon that cannot be minted fails naming the account and the machine.

When the command has not exited by `--timeout`, the daemon stops waiting and reports the timeout as a failure. A guest exit code other than zero becomes this command's exit status, so a script can test the guest command's result directly.

```sh
vmlab exec dc01 -- powershell -NoProfile -Command "Get-Service ntds | Select Status"
vmlab exec --user admin dc01 -- cmd /c whoami
vmlab exec --timeout 600 nix01 -- nix-build default.nix
```

Exit status is 0 when the guest command exits 0, and the guest's own exit code otherwise. `not_found` (4) means the lab declares no machine by that name. `failed` (1) covers a machine that is not running, an agent that does not answer, a login that cannot be minted, and a timeout. An empty command exits 1 before any request is sent.

## vmlab shell

Attaches an interactive shell inside a machine over the vmlab agent's virtio-serial channel. No guest network is involved and no guest sshd exists. Every attach opens a fresh terminal session, so several shells on one machine are independent. `vmlab container shell` is the same verb under the container noun.

```sh
vmlab shell [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `--user <LOGIN>` | Run as this login: the label a `login {}` block declares, or the account name as an alias. Default: the machine's default login. `SYSTEM` on Windows or `root` on Linux is the agent identity. |
| `--password <PASSWORD>` | Password for an account the lab file does not declare, or one whose declared password has been rotated. Requires `--user`. |
| `-h`, `--help` | Print help. |

The command reads the terminal's size, asks the lab daemon to open an agent terminal at that size under the chosen login, and connects the terminal to the unix socket the daemon exposes for the session. It prints `connected to "<machine>" — escape character is ^]` and then hands the terminal over: keystrokes go to the guest, and a resize on the host is forwarded to the session. The shell is a PTY on Linux and a ConPTY on Windows.

Ctrl-] detaches and leaves the guest shell to end on its own. Exiting the guest shell ends the session and returns to the prompt. The identity is the machine's default `login {}` when it declares one and the agent's own identity otherwise (logins-and-ssh.md); `--user` and `--password` behave as they do for `vmlab exec`.

For a dev machine, `vmlab ssh` and `vmlab dev attach` give the same shell over the SSH facade, which is what an editor uses. This verb needs no SSH client and no managed config block.

```sh
vmlab shell nix01
vmlab shell --user admin dc01
```

Exit status is 0 when the session ends normally. `not_found` (4) means the lab declares no machine by that name. `failed` (1) covers a machine that is not running, an agent that does not answer, and a login that cannot be minted.

## vmlab console

Attaches a VNC viewer to a VM's display. Every VM serves VNC on a unix socket in the lab's runtime directory, and this command either launches a viewer against that socket or bridges the socket to a localhost TCP port and prints the address. It is the interactive counterpart of the screen verbs in snapshots-vision.md.

```sh
vmlab console [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The VM, as `[lab/]name`. A bare name is looked up in the lab of the current directory. |
| `--tcp` | Forward the VNC display over TCP instead of launching a viewer. |
| `-h`, `--help` | Print help. |

The command requires the lab daemon to be running already and never starts one: a stopped lab is refused with `lab "<name>" is not running`. It then checks the VM's `vnc.sock` exists and refuses when it does not, which is what a VM that has not been started looks like. Containers have no display, so this verb is for VMs only.

The viewer comes from the `viewer` command template in the host configuration (host-profiles.md) when one is set. Otherwise the command looks on `PATH` for `remote-viewer`, then `gvncviewer`, then `vncviewer`. A configured viewer is handed the socket path directly. A detected viewer needs TCP, so the command spawns a detached helper that holds a unix-to-TCP bridge open, launches the viewer against it, and prints `opened <lab>/<vm> in a viewer (closes with the window)`. The terminal is free straight away, and the bridge ends when the viewer window closes.

With `--tcp`, or when no viewer is found, the command binds a localhost port itself, preferring a VNC display port from 5900 upward so a client that takes `host:display` works, and prints `VNC for <lab>/<vm> on 127.0.0.1:<port>`. It then holds the bridge until Ctrl-C. This is the path to use under WSL 2, where the viewer lives on the Windows side and connects to the forwarded port.

```sh
vmlab console winsrv
vmlab console --tcp mixed-lab/winsrv
```

Exit status is 0 on success. The command sends no request that can fail after the liveness check, so every failure, including a lab that is not running or a missing socket, exits 1.

## vmlab cp

Copies a file or a tree between the host and a guest over the vmlab agent's file session, with no guest network and no share involved. Either side may be a guest reference spelled `<vm>:<path>`; the other is a host path. Parent directories on the receiving side are created.

```sh
vmlab cp <SRC> <DEST>
```

| Option | Meaning |
| --- | --- |
| `<SRC>` | Source: a host path, or `<vm>:<path>` to pull from the guest. |
| `<DEST>` | Destination: `<vm>:<path>` to push, or a host path when pulling. |
| `-h`, `--help` | Print help. |

The guest side is recognised by splitting on the first colon, so a Windows path keeps its drive letter: `box:C:/weave` is the machine `box` and the guest path `C:/weave`. The `<vm>` part accepts the `[lab/]name` form. When neither side is a guest reference the command is refused locally. A push whose host source does not exist is refused before any request is sent.

A push sends one file, or walks a directory and sends every file under it to the matching path below the guest destination, one request per file, keeping each file's mode. The daemon opens the host file itself, so the path is made absolute first, and verifies the digest end to end. It prints `pushed <bytes> bytes to <vm>:<path>` for a file and `pushed <n> file(s), <bytes> bytes to <vm>:<path>` for a tree.

A pull copies one guest file to the host path. When the host destination is an existing directory the guest file's name is kept under it. It prints `pulled <bytes> bytes to <path>`.

Note — transfers run as the agent identity: `cp` still runs as SYSTEM or root even on a machine that declares a `login {}`, unlike `exec` and `shell`. A pushed file is owned by the agent identity, not by the login you would attach as. To write into a login's home as that login, use `scp` over the SSH alias, or the `as_login` handle in a provision script (logins-and-ssh.md).

```sh
vmlab cp ./tools/ dc01:C:/tools
vmlab cp dc01:C:/Windows/debug/netsetup.log ./logs/
vmlab cp mixed-lab/nix01:/etc/nixos/configuration.nix ./configuration.nix
```

Exit status is 0 on success. `not_found` (4) means the lab declares no machine by that name. `failed` (1) covers a machine that is not running, an agent that does not answer, a guest path that cannot be opened, and a digest mismatch. A malformed pair of arguments or a missing host source exits 1 before any request is sent.

## vmlab clipboard

Reads or writes a guest's clipboard over the guest agent channel. No guest network is involved, so it works on a machine with no NIC at all.

```sh
vmlab clipboard <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `get` | Write the guest clipboard to stdout, with no trailing newline added. |
| `set` | Set the guest clipboard from TEXT, or from stdin when TEXT is omitted. |
| `-h`, `--help` | Print help. |

The machine must be running with an agent that advertises the `clipboard` feature; see `vmlab machine capabilities`.

### vmlab clipboard get

```sh
vmlab clipboard get [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `--json` | Emit the raw JSON string instead of the bare text. |
| `-h`, `--help` | Print help. |

The guest's clipboard text is written to stdout byte for byte. No newline is appended, so what you pipe on is exactly what the guest held. With `--json` the text is printed as one JSON string literal instead. The agent is given 10 seconds to answer.

### vmlab clipboard set

```sh
vmlab clipboard set [OPTIONS] <MACHINE> [TEXT]
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `[TEXT]` | The text to copy. Omit it to read stdin. |
| `--json` | Emit the raw JSON reply instead of a confirmation. |
| `-h`, `--help` | Print help. |

With a `TEXT` argument that text becomes the guest's clipboard. Without one, stdin is read to end of file and passed through verbatim, trailing newline included, so `vmlab clipboard get a | vmlab clipboard set b` round-trips exactly. Use `echo -n` when you do not want the newline `echo` adds. On success the verb prints `copied N bytes to "<machine>" clipboard`; with `--json` it prints the daemon's reply, which is `true`.

### Examples

Move a password from one guest to another without typing it:

```sh
vmlab clipboard get dc01 | vmlab clipboard set client01
```

Paste a host file's content into a guest:

```sh
vmlab clipboard set client01 < notes.txt
```

Read a guest's clipboard into a shell variable:

```sh
token=$(vmlab clipboard get buildbox)
```

### Exit status

0 when the clipboard was read or written. Exit 4 (`not_found`) means the lab declares no machine by that name. A machine that is not running, an agent that is not answering or lacks the `clipboard` feature, and a request that times out all exit 1 (`failed`). A qualified reference to a lab that is not running exits 1. A usage error exits 2.

## vmlab snapshot

Takes, restores, lists and deletes snapshots of one machine or of the whole lab. A VM and a container snapshot identically. A snapshot records the machine's power state at capture: an online snapshot of a running machine carries disk, RAM and device state, and restoring it resumes the machine where it was; an offline snapshot carries disk state only and restoring it leaves the machine powered off.

```sh
vmlab snapshot <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `create` | Take a snapshot of one VM/container, or lab-wide with no `--vm`. |
| `restore` | Restore a snapshot (resumes running iff it was taken online). |
| `list` | List a VM's/container's snapshots. |
| `delete` | Delete a VM/container snapshot. |
| `-h`, `--help` | Print help. |

A bare name, or no machine at all, is resolved against the lab in the current directory, and the lab daemon is started if none is running.

Warning — snapshots are not a workspace backup: a dev machine's source lives on the host, which is what survives `destroy` and what a restore re-converges the guest from. Both `create` and `restore` print this sentence in their help, and their refusals are built on it.

### vmlab snapshot create

```sh
vmlab snapshot create [OPTIONS] <NAME>
```

| Option | Meaning |
| --- | --- |
| `<NAME>` | Snapshot name. |
| `--vm <VM>` | Machine (`[lab/]name`); omitted = every VM and container in the lab. |
| `-h`, `--help` | Print help. |

Takes a snapshot named `NAME` of one machine, or, with no `--vm`, of every VM and container in the lab under the one name. A lab-wide snapshot is taken machine by machine; consistency across machines is best effort, not coordinated. Each snapshot is recorded in the lab's state with whether it was online and when it was taken, and for a container with the image digest it was taken against. Prints `snapshot "<name>" created`.

For a dev machine with a workspace, capture is bracketed by the workspace syncer (dev-machines.md). A sync pass is flushed first, so the snapshot is coherent with the canonical copy. If the guest holds work the host has never seen, because the syncer is halted, has not completed a pass, or could not finish the flush, the capture **refuses**, naming the machine and the outstanding paths. There is no escape flag: a snapshot of a tree that exists nowhere else is not a thing to take against a warning. `vmlab dev sync flush` waits for a pending pass; a halt has to be resolved first with `vmlab dev sync resolve`. Every other machine passes straight through.

### vmlab snapshot restore

```sh
vmlab snapshot restore [OPTIONS] <NAME>
```

| Option | Meaning |
| --- | --- |
| `<NAME>` | Snapshot name. |
| `--vm <VM>` | Machine (`[lab/]name`); omitted = every VM and container in the lab. |
| `--discard-guest-changes` | Restore a machine whose workspace is halted, throwing the guest copy of every conflicting path away. |
| `-h`, `--help` | Print help. |

Rolls one machine, or every machine in the lab, back to the snapshot `NAME`. A machine with no snapshot by that name fails with `"<machine>" has no snapshot "<name>"`; in the lab-wide form the first such machine stops the whole restore. An online snapshot resumes running; an offline one leaves the machine stopped. A container's snapshot is pinned to the image digest it was taken against, and restoring it against a different pinned image is refused, naming both digests, with the remedies of destroying the machine or restoring the original pin. Prints `snapshot "<name>" restored`.

For a dev machine with a workspace, restore is bracketed too. A restore rewinds the guest by hundreds of files at once, which a bidirectional syncer cannot tell from the developer having edited them and would carry onto the canonical copy. So the syncer is taken off the workspace before the rewind, a note that a **re-seed** is owed is written to the sync ledger, and after the rewind the syncer goes back on and runs the re-seed: a host-only reconcile that carries the canonical tree back into the guest and emits no guest-to-host action at all, completing before the watch reopens. The note rides the ledger, so a daemon that dies mid-restore still owes it, and a restore that fails after the note is written still runs it, with the reason on the event feed.

A workspace that is **halted** at restore time is refused by default, because restoring would silently destroy the guest copy of every conflicting path. The refusal names the affected paths and this verb's one escape flag, `--discard-guest-changes`, which restores anyway and throws those guest copies away. `vmlab dev sync diff` shows the guest copy host-side before you decide.

### vmlab snapshot list

```sh
vmlab snapshot list <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

Prints one row per snapshot of the machine: `NAME`, `KIND` (`online` or `offline`) and `TAKEN`, the capture time in UTC. A machine with none, including a name the lab does not declare, prints `no snapshots for <name>`.

### vmlab snapshot delete

```sh
vmlab snapshot delete <VM> <NAME>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `<NAME>` | Snapshot name. |
| `-h`, `--help` | Print help. |

Removes the snapshot from the machine's disk and from the lab's record. Prints `snapshot "<name>" deleted`.

### Examples

Checkpoint a domain controller before a risky change:

```sh
vmlab snapshot create --vm dc01 pre-upgrade
vmlab snapshot list dc01
```

```sh
NAME                     KIND     TAKEN
pre-upgrade              online   2026-09-02T09:14:31Z
```

Roll the whole lab back to a known state:

```sh
vmlab snapshot restore clean
```

Restore a dev machine whose workspace is halted, giving up the guest's copies:

```sh
vmlab snapshot restore --vm dev01 --discard-guest-changes before-refactor
```

### Exit status

0 on success, and 0 for `list` on a machine with no snapshots. Every refusal and failure exits 1 (`failed`): an unknown machine, a missing snapshot, a capture refused because the workspace is not in step, a restore refused on a halted workspace or a changed image pin, and a snapshot the machine cannot take or restore. Exit 5 (`conflict`) means the supervisor tracks a lab with this name from another directory. A usage error exits 2.

## vmlab dev

Holds the verbs that mean nothing for a machine that is not `@dev`: attaching to a dev machine from cold, recording which dev machine is yours, and reading or resolving the workspace syncer (dev-machines.md). The SSH facade is a general capability, so `vmlab ssh` and `vmlab ssh-config` stay top level.

```sh
vmlab dev <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `attach` | Up the dev machine, wait until it is attachable, and become a shell on it. |
| `use` | Record which dev machine is yours, in the lab's gitignored `.vmlab/`. |
| `sync` | The workspace syncer: what it is doing, and what to do about a halt. |
| `-h`, `--help` | Print help. |

Every verb here that takes an optional machine resolves it through one ladder: the argument, then `VMLAB_DEV_MACHINE`, then the `dev use` selection, then the lab's default `@dev` machine. A rung that names a machine which is not a dev machine in this lab is an error that says which rung and lists the candidates. When no rung names one and no machine is the default, the error lists the dev machines and the three ways to pick one. A lab with no `@dev` machine at all says so.

Exit status is 0 on success. The selection ladder, the lab file, and the managed SSH block are checked locally and fail with 1. Requests the verbs send carry the wire protocol's error codes: `not_found` (4) when the daemon does not know the machine, and `failed` (1) for everything the syncer refuses, since the workspace verbs answer every refusal as `failed` with the reason in the message.

### vmlab dev attach

```sh
vmlab dev attach [MACHINE]
```

| Option | Meaning |
| --- | --- |
| `[MACHINE]` | Which dev machine. Default: `VMLAB_DEV_MACHINE`, then the `vmlab dev use` selection, then the lab's default `@dev` machine. |
| `-h`, `--help` | Print help. |

`attach` is cold to editing in one command. It validates the lab file, resolves the machine, refreshes the managed SSH block and checks this machine's alias with `ssh -G`, all before anything boots, so an attach that could not work fails before a machine is started. It prints which machine it is attaching to and which rung chose it, so a surprise can be interrupted before the wrong guest boots.

It then ups this machine alone, not the lab, streaming `up`'s output, and waits until the machine is `attachable` as `vmlab machine capabilities` reports it. The wait is visible: each distinct stage is printed once, such as waiting for the machine to run, to become ready, or for its agent to answer. A running, ready machine whose agent answers without the `tunnel` and `fileops` features is refused with both remedies, rebuilding the template or `vmlab machine repair-agent`. The wait gives up after 300 seconds past `up` and says what it was still waiting for.

On `attachable` it prints the SSH alias and every labelled alias beside it, a reminder that vmlab launches no editor, a note that agent forwarding refuses silently, a note that the workspace syncer belongs to the lab daemon and survives the shell when the machine declares a workspace, and the same editor snippet and offline notes as `ssh-config --print`. Then the process becomes `ssh <alias>`. Nothing of vmlab survives to own anything the editor still needs.

```sh
cd examples/dev-vscode-windows
vmlab dev attach
vmlab dev attach dev01
```

Exit status is `ssh`'s once the shell has started. Before that, the ladder, validation, and the block check exit 1; the `up` and the capability probe carry `failed` (1) and `not_found` (4); a refused or timed-out wait exits 1.

### vmlab dev use

```sh
vmlab dev use <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The dev machine to record. |
| `-h`, `--help` | Print help. |

`use` records which dev machine is yours in the lab's own gitignored `.vmlab/` directory, in the file `dev-machine`. The lab file is committed and shared, so it cannot say this; the selection is per-developer by construction. The verb validates the lab first, because a selection against a lab that does not validate is a promise vmlab cannot keep, then checks the name through the same ladder `attach` uses, so what `use` accepts and what `attach <machine>` accepts are one answer. It needs no daemon and starts nothing.

It prints the lab, the machine, and the file the selection went in, and reminds you that `vmlab destroy` forgets it with everything else in `.vmlab/`, and that running `use` again changes it.

```sh
vmlab dev use buildbox
```

Exit status is 0 on success and 1 for a lab that does not validate or a name that is not a dev machine.

### vmlab dev sync

```sh
vmlab dev sync <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `status` | What the syncer last decided: halted paths, warnings, and what it skipped by name. |
| `flush` | Run a sync pass now and wait for it, rather than for the next edit. |
| `diff` | Show the guest's copy of a path beside the host's. |
| `resolve` | Pick which side wins at a halted path, and carry it out. |
| `-h`, `--help` | Print help. |

Resolution is host-side, necessarily. The host opens channels and the guest answers, so there is no guest-to-host control path: a `vmlab` inside the dev machine could not call back. These verbs are typed in the lab directory, not in the shell `attach` drops you into. The guest's only signal of a halt is the marker file `.vmlab-sync-halt` at its workspace root, which lists the halted paths.

All four verbs talk to a lab daemon that is already running and never start one: a syncer exists only while its machine is up, so starting a daemon would boot a lab in order to answer no. A lab that is not running is refused with a pointer to `vmlab up` and `vmlab dev attach`. A machine that is up but declares no `@dev(workspace = …)`, or is not up, is refused with `has no workspace syncer running`.

### vmlab dev sync status

```sh
vmlab dev sync status [MACHINE]
```

| Option | Meaning |
| --- | --- |
| `[MACHINE]` | Which dev machine. Default: `VMLAB_DEV_MACHINE`, then the `vmlab dev use` selection, then the lab's default `@dev` machine. |
| `-h`, `--help` | Print help. |

`status` reads the syncer's report off the lab status projection, the same value the console shows, and prints it in the order it matters. First the halt, whole, when there is one: the headline names the machine and how many paths conflict, or the bulk-delete guard that tripped, then each halted path with its reason, a count of any not listed, and the routes out. Without a halt the first line says the workspace is in step with its pass count, or is a number of paths behind the canonical copy, or has not completed a pass yet.

Then everything the syncer declined to do or is waiting on, each by name: a `waiting` line while both directions wait on a stat-walk, a `re-seeding` line while the bracket after a snapshot restore runs, a `volume` warning naming a subtree that carried an unusual amount of work, a `trouble` line when the last pass could not finish, the count of watch discontinuities answered with a full walk, `.git` paths deferred while a lock is held, paths not yet carried across that a snapshot capture would refuse on, and paths not synced by name with their reasons, such as a socket or a file over the size guard.

```sh
$ vmlab dev sync status
the workspace on "dev01" has stopped, both directions, on 2 conflicting paths

  src/main.rs
      both sides changed it since they last agreed
  README.md
      both sides changed it since they last agreed

`vmlab dev sync diff <path>` shows the guest copy host-side; `vmlab dev sync resolve <path> --host` or `--guest` picks a side, and `--all` takes the batch. Making both sides identical by hand needs no verb at all — the next pass adopts them as agreed. Both copies are still where they were: a halt writes neither and deletes neither.
```

Exit status is 0 on success and 1 when the lab is not running or the machine has no syncer.

### vmlab dev sync flush

```sh
vmlab dev sync flush [MACHINE]
```

| Option | Meaning |
| --- | --- |
| `[MACHINE]` | Which dev machine, through the same ladder as `status`. |
| `-h`, `--help` | Print help. |

`flush` asks the syncer to run a pass now, waits for that pass to complete, and prints the same report `status` prints, so the effect of the flush is visible. It is the pass a snapshot capture runs first, and the way to make a host edit land before the debounce would. A halted workspace stays halted: a flush carries nothing across a halt. The wait gives up after 120 seconds and says to check `vmlab status` for whether the machine is still answering.

```sh
vmlab dev sync flush dev01
```

Exit status is 0 on success and `failed` (1) when the pass does not complete or the machine has no syncer.

### vmlab dev sync diff

```sh
vmlab dev sync diff [OPTIONS] [PATHS]...
```

| Option | Meaning |
| --- | --- |
| `[PATHS]...` | Workspace-relative paths. Default: every halted path. |
| `--machine <MACHINE>` | Which dev machine, through the same ladder as `status`. |
| `-h`, `--help` | Print help. |

`diff` brings the guest's copy of each path to the host and shows it beside the host's. The host copy is a directory on this workstation; only the guest's is behind the seam, which is why the verb exists rather than attaching and looking. With no path and no halt it is refused, because there is nothing to name.

The output opens with the host root and the guest root, then one section per path. Two identical copies say so, and that the next pass adopts them with no verb. A path one side lacks says which. Two readable text copies print as a unified diff, `-` for host lines and `+` for guest lines, up to 2000 lines a side; past that, or for a binary or a file the guest would not send whole, each side prints its size and digest prefix and why it was not shown.

```sh
vmlab dev sync diff
vmlab dev sync diff src/main.rs --machine dev01
```

Exit status is 0 on success and `failed` (1) when no path is named and nothing is halted, or the guest copy cannot be read.

### vmlab dev sync resolve

```sh
vmlab dev sync resolve [OPTIONS] [PATHS]...
```

| Option | Meaning |
| --- | --- |
| `[PATHS]...` | Workspace-relative paths. Omit with `--all`. |
| `--host` | The canonical host copy wins: carry it into the guest. |
| `--guest` | The guest's working copy wins: carry it onto the canonical copy. |
| `--all` | Every halted path, as the halt currently stands. |
| `--machine <MACHINE>` | Which dev machine, through the same ladder as `status`. |
| `-h`, `--help` | Print help. |

`resolve` records which side wins at the named paths and runs a pass that carries it out, then prints the report. There is no default side: exactly one of `--host` and `--guest` is required, and the two conflict. Paths are required unless `--all` is given, which the daemon expands from the halt it is holding, so a thirty-thousand-file halt needs no list. `--all` on a workspace that is not halted is refused with `there is nothing to resolve`. Named paths are recorded as given, and the pass that follows carries the chosen side across at each of them.

Warning — the losing copy is overwritten and is not recoverable from vmlab. Run `vmlab dev sync diff` first. Making both sides identical by hand is a third route that needs no verb: the next pass adopts them as agreed.

```sh
vmlab dev sync resolve src/main.rs --guest
vmlab dev sync resolve --all --host
```

Exit status is 0 on success, 1 when no side or no path is given, and `failed` (1) when the workspace is not halted or the pass does not complete.

## vmlab ssh

Attaches to a machine over the SSH facade the lab daemon terminates on the host (logins-and-ssh.md). It refreshes the managed block in `~/.ssh/config`, checks the machine is running, and then replaces itself with the system `ssh` against the machine's alias. It is not a second SSH client: the `ssh` in play is the one your editor uses, with your own `Host *` settings, so a failure here reproduces as `ssh <alias>`.

```sh
vmlab ssh <MACHINE> [-- <CMD>...]
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | `[lab/]machine`: a bare name inside a lab directory, or the qualified form from anywhere. |
| `[CMD]...` | Command and arguments to run instead of a shell, after `--`. |
| `-h`, `--help` | Print help. |

A bare name is looked up in the lab of the current directory. The qualified `lab/machine` form is resolved through the supervisor's lab registry, so it works from any directory but only for a lab whose daemon is running. The lab file must declare the machine.

The refresh of the managed block fails hard here, where every other verb only warns, because the alias is the command. After writing, the command runs `ssh -G` on this machine's alias and refuses when something earlier in the config, an `Include`, or a system file resolves a different `ProxyCommand`; the refusal names the file and line that won. The alias is `vmlab-<lab>-<machine>`, with `-<label>` appended for each declared login.

The command refuses on a stopped machine and never starts one, like `console` and `exec`. A lab with no running daemon, or a machine whose state is anything but running, is refused with a message that says to run `vmlab up <machine>` first. A daemon that predates an edit adding the machine is refused with a pointer to `vmlab lab restart`.

After the checks the process becomes `ssh <alias> [CMD...]`. From then on signals, the terminal, the `~.` escape, and the exit code are the client's. The `ProxyCommand` the alias carries is the hidden `vmlab ssh-proxy`, which pipes the connection onto the facade and does no lifecycle of its own. Agent forwarding is not served and fails silently in the guest; `ssh -R` is refused by the facade.

```sh
vmlab ssh dev01
vmlab ssh dev-vscode-windows/dev01 -- hostname
scp ./notes.md vmlab-dev-vscode-windows-dev01:notes.md
```

Exit status is the exit status of `ssh` once it has been started. Before that, every refusal exits 1: a lab or machine that is not declared, a managed block that could not be written or that `ssh -G` resolves elsewhere, a lab that is not running, and a machine that is not running.

## vmlab ssh-config

Refreshes the managed block vmlab keeps in `~/.ssh/config` for the lab of the current directory, and with `--print` emits one machine's stanzas together with the editor settings snippet. The block is vmlab's whole host-side SSH footprint: one `Host` stanza per declared machine and per declared login, each with a `ProxyCommand` onto the SSH facade (logins-and-ssh.md).

```sh
vmlab ssh-config [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--print <MACHINE>` | Print one machine's stanza and the editor settings snippet, for a client that will not read the file. |
| `-h`, `--help` | Print help. |

Any command that loads a lab refreshes the block on the way past and only warns when that fails. This verb exists for the two moments the ambient refresh cannot serve: when you want to know the block is current, and when your client will not read `~/.ssh/config` at all. Its own failure is an error, because the write is what was asked for.

The block is generated from the lab file, not from running machines, so every declared machine has an alias whether or not it is up. Aliases are `vmlab-<lab>-<machine>` for the machine's default identity and `vmlab-<lab>-<machine>-<label>` for each `login {}` label, so attaching as another account is a pick in an editor's host list. A login whose label is not one `ssh_config` word gets no alias, and the command says so on standard error with the `ssh -l` form to use instead. The write takes a lock, renames a temporary file onto the resolved path, and checks the first alias with `ssh -G`. Where the block lives is a field in the host configuration (host-profiles.md).

Without `--print` the command prints the config path, whether the block was updated or was already current, and how many aliases the lab contributes. With `--print` it prints the machine's stanzas as they appear in the block, then the VS Code `settings.json` snippet, which sets `remote.SSH.localServerDownload` to `always` and, for a Windows guest, `remote.SSH.remotePlatform` per alias, and then two notes: personal config copies over the alias with `scp`, and a host-side service is reached by giving the machine a NIC on a segment with egress rather than by `ssh -R`, which the facade refuses. `vmlab dev attach` prints the same handover.

```sh
$ vmlab ssh-config
/home/wil/.ssh/config — block already current (4 aliases for lab "dev-vscode-windows")
$ vmlab ssh-config --print dev01
```

Exit status is 0 on success. The command sends no daemon request, so every failure exits 1: no lab in the current directory, a lab file that does not load, a block that cannot be written, an `ssh -G` check that resolves another `ProxyCommand`, or a `--print` machine the lab does not declare.
