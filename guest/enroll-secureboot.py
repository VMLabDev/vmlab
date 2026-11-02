#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["virt-firmware==26.9"]
# ///
"""Create an offline Microsoft-plus-lab OVMF VARS template from public certificates."""

import argparse
import hashlib
import json
import os
import tempfile
import uuid
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from virt.firmware.varstore.edk2 import Edk2VarStore


def snapshot(variable):
    return (
        variable.data,
        variable.attr,
        variable.guid,
        variable.bytes_time(),
        variable.count,
        variable.pkidx,
    )


def enroll(template, certificates, output, owner=None):
    template = Path(template)
    output = Path(output)
    if not certificates:
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
    db_time = variables["db"].time
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
    variables["db"].time = db_time
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
            if name != "db" and snapshot(enrolled[name]) != original:
                raise ValueError(f"enrollment changed {name}")
        if not enrolled["db"].data.startswith(before["db"][0]):
            raise ValueError("enrollment removed existing db entries")
        if snapshot(enrolled["db"])[1:] != before["db"][1:]:
            raise ValueError("enrollment changed db metadata")
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
    return {
        "template_sha256": hashlib.sha256(template.read_bytes()).hexdigest(),
        "output_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
        "certificate_sha256": fingerprints,
        "preserved": [
            "PK",
            "KEK",
            "dbx",
            "Microsoft db certificates",
            "other variables",
        ],
    }


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
        required=True,
        help="public X.509 PEM/DER certificate; repeat for multiple lab signers",
    )
    parser.add_argument(
        "--owner",
        type=uuid.UUID,
        help="signature owner GUID; default derived from certificate",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="new offline VARS template; must not exist",
    )
    args = parser.parse_args()
    try:
        report = enroll(args.template, args.certificate, args.output, args.owner)
    except (ValueError, OSError) as error:
        parser.exit(1, f"enroll-secureboot: {error}\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
