# Serve the web console

## Purpose

Run vmlab-web and manage a lab from the browser instead of the CLI.

## Prerequisites

- vmlab is installed with the `web` feature (the official container image includes it).
- For a non-loopback bind: a username and password (or argon2 hash) to protect the port.

## Flowchart

![diagram](../_wdoc/process_serve_web_console-diagram-1.svg)

## Steps

### Step 1: Launch vmlab-web

```console
$ cd my-lab/                      # the directory holding vmlab.wcl
$ vmlab-web --up                  # local-only on http://127.0.0.1:7878, lab boots in the background

$ vmlab-web --bind 0.0.0.0 --user admin \
    --password-hash "$VMLAB_WEB_PASSWORD_HASH"     # exposed on the network: login required
```

> [!NOTE]
> **Secure default**
> A non-loopback bind with no credentials is refused at startup; pass `--no-auth` only when something else (a VPN, an ingress proxy with its own auth) guards the port. Behind a reverse proxy add `--trust-proxy` so login rate-limiting sees real client addresses.

Start `vmlab-web` from the lab directory ([flags](../references/entity_vmlab_web.md)); `--up` brings that lab up in the background while the server starts serving immediately. In the official container image this is the default command — the compose stack maps :7878 and feeds the auth env vars.

### Step 2: Open the console and pick a lab

Browse to the printed URL and sign in if prompted. The topbar's lab switcher lists running and managed labs — pick one, or **New lab…** to scaffold a fresh managed lab in the designer. Opening the working-directory lab shows its overview: machine cards, power controls, events, and launch cards for declared [guest web pages](../references/entity_web_block.md).

### Step 3: Work in the console

Edit topology in the lab editor's **Overview** (designer canvas + inspector), files in **Files**, and watch **Logs**. Machine pages give the live desktop (**Console**), an agent shell (**Terminal**), guest metrics and clipboard; the **Templates** page builds and publishes templates with live build consoles; **Playbook** tabs run config-weave check/apply. The [tour](../references/concept_web_console.md) maps every tab.

> [!TIP]
> **Verification**
> The overview shows the lab's machines cycling to ready; a machine page's Console tab shows the live desktop, and its Terminal tab opens an agent shell.

## Related

- [vmlab-web](../references/entity_vmlab_web.md)

- [The web console](../references/concept_web_console.md)

- [Run vmlab in a container](../references/process_run_in_container.md)

- [Bring a lab up and tear it down](../references/process_golden_path.md)

[← Back to SKILL.md](../SKILL.md)
