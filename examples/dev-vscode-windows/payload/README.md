# payload/ — bytes for the offline guest

Drop a `.vsix` here as `extension.vsix` and `scripts/editor-bits.ws` stages
it into `%USERPROFILE%\vsix\` on dev01, as `PROBE\dev`. Nothing is staged if
the file is absent; the rest of the provision is unaffected.

Get one without a marketplace round trip on the guest:

```sh
# from the host, which is the side with the network
curl -L -o extension.vsix \
  'https://marketplace.visualstudio.com/_apis/public/gallery/publishers/<pub>/vsextensions/<name>/<version>/vspackage'
```

Then, in a terminal on the attached machine (VS Code's integrated terminal,
or `vmlab dev attach`):

```powershell
code --install-extension $env:USERPROFILE\vsix\extension.vsix
```

`code` on the remote is the Remote-SSH server's own CLI shim, so it exists
only once a client has attached at least once — which is the observation
recorded in the example's README under *Install-from-VSIX over the facade*.

The other route for bytes is `media {}` (§6.3), whose stated primary use is
already payload delivery to guests with no network: the folder becomes an
ISO, the ISO becomes a drive letter, and the provision copies off it. Use
that when the payload is large or you would rather not have it in the repo.

**vmlab moves bytes it is told to move and never interprets them.** `media {}`
does not know a VSIX from a driver bundle, and `provision {}` does not know
`code --install-extension` from `winget install`.
