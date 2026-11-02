#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["virt-firmware==26.9"]
# ///
"""Build or inspect an offline OVMF trust store from public certificates and pinned inputs."""

import argparse
import hashlib
import io
import json
import os
import tarfile
import tempfile
import urllib.request
import uuid
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from virt.firmware.efi import guids, siglist
from virt.firmware.varstore.edk2 import Edk2VarStore


def inspect_store(template):
    variables = Edk2VarStore(str(template)).get_varlist()
    return {
        name: {
            "data_sha256": hashlib.sha256(variables[name].data).hexdigest(),
            "lists": [
                {
                    "type": str(group.guid),
                    "entries": [
                        {
                            "owner": str(entry["guid"]),
                            "sha256": (
                                group.x509.fingerprint(hashes.SHA256()).hex()
                                if group.x509
                                else entry["data"].hex()
                            ),
                            **(
                                {
                                    "subject": group.x509.subject.rfc4514_string(),
                                    "not_after": group.x509.not_valid_after_utc.isoformat(),
                                }
                                if group.x509
                                else {}
                            ),
                        }
                        for entry in group
                    ],
                }
                for group in variables[name].sigdb
            ],
        }
        for name in ("PK", "KEK", "db", "dbx")
    }


def read_pinned(artifact, cache):
    digest = artifact["sha256"]
    if len(digest) != 64 or any(
        character not in "0123456789abcdef" for character in digest
    ):
        raise ValueError("artifact SHA256 must be 64 lowercase hexadecimal digits")
    if not artifact["url"].startswith("https://"):
        raise ValueError("Microsoft inputs must use HTTPS")
    path = cache / digest
    if path.exists():
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(artifact["url"], timeout=60) as response:
            data = response.read(2 * 1024 * 1024 + 1)
    if len(data) > 2 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != digest:
        raise ValueError(f"digest or size mismatch: {artifact['url']}")
    if not path.exists():
        with tempfile.NamedTemporaryFile(dir=cache, delete=False) as temporary:
            temporary.write(data)
            temporary_path = Path(temporary.name)
        try:
            os.replace(temporary_path, path)
        finally:
            temporary_path.unlink(missing_ok=True)
    return data


def signature_entries(variable):
    return {
        (str(group.guid), str(entry["guid"]), entry["data"])
        for group in variable.sigdb
        for entry in group
    }


def add_microsoft(variables, manifest_path, template, cache):
    manifest = json.loads(Path(manifest_path).read_text())
    if manifest["schema_version"] != 1:
        raise ValueError("unsupported Microsoft manifest schema")
    if hashlib.sha256(template.read_bytes()).hexdigest() != manifest["template_sha256"]:
        raise ValueError(
            "Microsoft template builds require the pinned pristine Debian VARS"
        )
    cache.mkdir(parents=True, exist_ok=True)
    read_pinned(manifest["license"], cache)
    owner = uuid.UUID(manifest["owner"])
    for artifact in manifest["certificates"]:
        name = artifact["variable"]
        if name not in ("KEK", "db"):
            raise ValueError("Microsoft certificate target must be KEK or db")
        data = read_pinned(artifact, cache)
        certificate = x509.load_der_x509_certificate(data)
        variables[name].sigdb_add_cert_data(
            owner, certificate.public_bytes(serialization.Encoding.DER)
        )
    artifact = manifest["dbx"]
    with tarfile.open(
        fileobj=io.BytesIO(read_pinned(artifact, cache)), mode="r:gz"
    ) as archive:
        member = archive.getmember(artifact["member"])
        if not member.isfile() or member.size > 128 * 1024:
            raise ValueError("invalid dbx archive member")
        data = archive.extractfile(member).read()
    if hashlib.sha256(data).hexdigest() != artifact["member_sha256"]:
        raise ValueError("dbx member digest mismatch")
    database = siglist.EfiSigDB(data)
    if any(str(group.guid) != guids.EfiCertSha256 for group in database):
        raise ValueError("compatible dbx must contain only SHA256 image revocations")
    if sum(len(group) for group in database) != artifact["entries"]:
        raise ValueError("dbx entry count mismatch")
    variables["dbx"].sigdb.merge(database)
    variables["dbx"].data = bytes(variables["dbx"].sigdb)
    return manifest


def snapshot(variable):
    return (
        variable.data,
        variable.attr,
        variable.guid,
        variable.bytes_time(),
        variable.count,
        variable.pkidx,
    )


def enroll(
    template, certificates, output, owner=None, *, microsoft_manifest=None, cache=None
):
    template = Path(template)
    output = Path(output)
    if not certificates and microsoft_manifest is None:
        raise ValueError("at least one public certificate is required")
    if output.exists():
        raise ValueError(
            f"output already exists: {output}; existing stores are never overwritten"
        )
    if template.stat().st_size != 540672:
        raise ValueError("the input must be a 540672-byte x86_64 OVMF VARS template")
    store = Edk2VarStore(str(template))
    variables = store.get_varlist()
    for name in ("PK", "KEK", "db", "dbx"):
        if name not in variables or not variables[name].data:
            raise ValueError(f"the input has no enrolled {name}")
    microsoft = [
        item
        for item in variables["db"].sigdb
        if item.x509 is not None and "Microsoft" in item.x509.subject.rfc4514_string()
    ]
    if not microsoft:
        raise ValueError("the input db has no Microsoft certificate")
    before = {name: snapshot(variable) for name, variable in variables.items()}
    original_entries = {
        name: signature_entries(variables[name]) for name in ("KEK", "db", "dbx")
    }
    original_times = {name: variables[name].time for name in ("KEK", "db", "dbx")}
    changed = {"db"}
    microsoft = None
    if microsoft_manifest is not None:
        microsoft = add_microsoft(
            variables,
            microsoft_manifest,
            template,
            Path(cache) if cache else Path(__file__).parent / ".cache/secureboot",
        )
        changed.update(("KEK", "dbx"))
    fingerprints = []
    for path in certificates:
        data = Path(path).read_bytes()
        certificate = (
            x509.load_pem_x509_certificate(data)
            if data.startswith(b"-----BEGIN")
            else x509.load_der_x509_certificate(data)
        )
        fingerprint = certificate.fingerprint(hashes.SHA256()).hex()
        certificate_owner = owner or uuid.uuid5(
            uuid.NAMESPACE_URL, "https://vmlab.dev/secureboot/" + fingerprint
        )
        variables["db"].sigdb_add_cert_data(
            certificate_owner, certificate.public_bytes(serialization.Encoding.DER)
        )
        fingerprints.append(fingerprint)
    # This is an offline pristine template, not an authenticated runtime update.
    for name, timestamp in original_times.items():
        variables[name].time = timestamp
    with tempfile.NamedTemporaryFile(
        prefix=".vmlab-vars-", dir=output.parent, delete=False
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        store.write_varstore(str(temporary_path), variables)
        if temporary_path.stat().st_size != template.stat().st_size:
            raise ValueError("enrollment changed the flash layout")
        enrolled = Edk2VarStore(str(temporary_path)).get_varlist()
        if enrolled.keys() != variables.keys():
            raise ValueError("enrollment changed the variable set")
        for name, original in before.items():
            if name not in changed and snapshot(enrolled[name]) != original:
                raise ValueError(f"enrollment changed {name}")
        for name in changed:
            if not original_entries[name] <= signature_entries(enrolled[name]):
                raise ValueError(f"enrollment removed existing {name} entries")
            if snapshot(enrolled[name])[1:] != before[name][1:]:
                raise ValueError(f"enrollment changed {name} metadata")
        for fingerprint in fingerprints:
            if not any(
                item.x509 is not None
                and item.x509.fingerprint(hashes.SHA256()).hex() == fingerprint
                for item in enrolled["db"].sigdb
            ):
                raise ValueError(f"certificate was not enrolled: {fingerprint}")
        temporary_path.chmod(0o644)
        os.link(temporary_path, output)
    finally:
        temporary_path.unlink(missing_ok=True)
    report = {
        "template_sha256": hashlib.sha256(template.read_bytes()).hexdigest(),
        "output_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
        "certificate_sha256": fingerprints,
        "preserved": [
            "PK",
            "existing KEK entries",
            "existing dbx entries",
            "Microsoft db certificates",
            "other variables",
        ],
    }
    if microsoft is not None:
        report["microsoft_release"] = microsoft["release"]
        report["microsoft_manifest_sha256"] = hashlib.sha256(
            Path(microsoft_manifest).read_bytes()
        ).hexdigest()
        report["variables"] = inspect_store(output)
        report["microsoft_inputs"] = microsoft
        report["license_text"] = read_pinned(
            microsoft["license"],
            Path(cache) if cache else Path(__file__).parent / ".cache/secureboot",
        ).decode()
        report["tool_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        report["lock_sha256"] = hashlib.sha256(
            Path(__file__).with_suffix(".py.lock").read_bytes()
        ).hexdigest()
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--template",
        type=Path,
        default=Path(__file__).parent / "dist/firmware/x86_64/OVMF_VARS_4M.ms.fd",
    )
    parser.add_argument(
        "--certificate",
        type=Path,
        action="append",
        default=[],
        help="public X.509 PEM/DER certificate; repeat for multiple lab signers",
    )
    parser.add_argument(
        "--microsoft-manifest",
        type=Path,
        help="pinned Microsoft inputs for a pristine template build",
    )
    parser.add_argument("--cache", type=Path, help="verified Microsoft download cache")
    parser.add_argument(
        "--inspect", type=Path, help="list every PK/KEK/db/dbx entry without writing"
    )
    parser.add_argument(
        "--owner",
        type=uuid.UUID,
        help="signature owner GUID; default derived from certificate",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="new offline VARS template; must not exist",
    )
    args = parser.parse_args()
    try:
        if args.inspect:
            print(json.dumps(inspect_store(args.inspect), indent=2))
            return
        if args.output is None:
            parser.error("--output is required unless --inspect is used")
        report = enroll(
            args.template,
            args.certificate,
            args.output,
            args.owner,
            microsoft_manifest=args.microsoft_manifest,
            cache=args.cache,
        )
    except (ValueError, OSError, KeyError, tarfile.TarError) as error:
        parser.exit(1, f"enroll-secureboot: {error}\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
