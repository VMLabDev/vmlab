# config-weave guest binaries

vmlab's `playbook {}` blocks push a [config-weave] binary into each guest and
run `config-weave check|apply` there. The Compose stack mounts this directory
read-only at the container's default lookup path
(`/root/.local/share/config-weave/bin`), so put the two release binaries here:

```
docker/config-weave/
  config-weave-linux-x86_64
  config-weave-windows-x86_64.exe
```

Build them from the public repo (needs `cross` + a container runtime):

```sh
git clone https://github.com/Configweave/config-weave
cd config-weave
just release
cp dist/config-weave-linux-x86_64 dist/config-weave-windows-x86_64.exe \
   ../vmlab/docker/config-weave/
```

While this directory is empty, labs without `playbook {}` blocks work
normally; a lab that declares one (e.g. the `ad-demo` sample) fails its
preflight with a message pointing back here.

[config-weave]: https://github.com/Configweave/config-weave
