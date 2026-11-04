# Release the guest package

Run win3d scripts from `guest/win3d/`. Its version comes from
`installer/VERSION`; release tags on VMLabDev/vmlab are `win3d-v<version>`.
The vmlab CLI and hypervisor keep their separate `v*` release line.

## Prerequisites

The signing directory is `~/.config/vmlab/signing/win3d/`, mode 0700.
`WIN3D_SIGNING_DIR` overrides it. It contains test-signing.key, .crt, .cer,
SIGN_TIME and CERT-INFO.txt. Keep an offline backup of the private key.
No signing key belongs in the checkout.

The guest package needs Secure Boot off and Windows test signing.
Use `hypervisor = "vmlab"` with the vmlab-hv bundle. The bundle's shared
virglrenderer build includes the NVIDIA fence and memory-import fixes.
No QEMU or standalone render-server asset is built or released here.

## Package without publishing

Build the signed package with `scripts/build.sh`. This builds all driver
components and can take about an hour.

Run `scripts/release.sh --dry-run` to package that existing signed output.
Set `WIN3D_PACKAGE_DIR` to an existing bundle directory to package it without
copying build output into the checkout. The command checks every manifest
hash and the signing certificate, then produces a guest zip, release notes
and SHA256SUMS under out/release/. It never creates a tag or release.

Both the regular package and release zip include licenses/ and
THIRD_PARTY_NOTICES. Licence texts cover the KMD, Mesa, vkd3d-proton,
dxil-spirv, DXVK and the Vulkan loader.

The clean-tree check, source commit and source epoch are scoped to
guest/win3d/. Unrelated monorepo changes do not block guest packaging.

## Publish an approved release

After review, `scripts/release.sh <version> --publish` builds the signed
package, creates the annotated win3d tag, pushes that tag over HTTPS and
creates a release on VMLabDev/vmlab. A version with a suffix is a prerelease.
The default invocation builds assets without publishing.

Historical host assets and their build procedure remain in the archived
vmlab-win3d repository. They are not prerequisites for vmlab-hv.
