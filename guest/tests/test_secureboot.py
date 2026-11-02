"""Exercise offline enrollment against the shipped Microsoft VARS template."""

import datetime
import hashlib
import importlib.util
import io
import json
import shutil
import tarfile
import tempfile
import unittest
import uuid
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

GUEST = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("enroll", GUEST / "enroll-secureboot.py")
ENROLL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ENROLL)


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="vmlab-enrollment-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.template = GUEST / "dist/firmware/x86_64/OVMF_VARS_4M.ms.fd"
        self.key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        subject = x509.Name(
            [x509.NameAttribute(NameOID.COMMON_NAME, "vmlab ephemeral test signer")]
        )
        self.certificate = (
            x509.CertificateBuilder()
            .subject_name(subject)
            .issuer_name(subject)
            .public_key(self.key.public_key())
            .serial_number(1)
            .not_valid_before(datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC))
            .not_valid_after(datetime.datetime(2027, 1, 1, tzinfo=datetime.UTC))
            .sign(self.key, hashes.SHA256())
        )
        self.pem = self.root / "signer.pem"
        self.pem.write_bytes(self.certificate.public_bytes(serialization.Encoding.PEM))

    def test_preserves_microsoft_and_other_variables_reproducibly(self):
        original = self.template.read_bytes()
        first, second = self.root / "first.fd", self.root / "second.fd"
        report = ENROLL.enroll(self.template, [self.pem], first)
        ENROLL.enroll(self.template, [self.pem], second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(self.template.read_bytes(), original)
        self.assertEqual(first.stat().st_size, 540672)
        self.assertEqual(
            report["certificate_sha256"],
            [self.certificate.fingerprint(hashes.SHA256()).hex()],
        )

    def test_existing_output_is_never_replaced(self):
        target = self.root / "existing.fd"
        target.write_bytes(b"existing guest state")
        with self.assertRaisesRegex(ValueError, "already exists"):
            ENROLL.enroll(self.template, [self.pem], target)
        self.assertEqual(target.read_bytes(), b"existing guest state")

    def test_der_and_pem_have_identical_enrollment(self):
        der = self.root / "signer.der"
        der.write_bytes(self.certificate.public_bytes(serialization.Encoding.DER))
        owner = uuid.UUID("34ad309f-6107-4bb0-8d36-c803559ac510")
        first, second = self.root / "pem.fd", self.root / "der.fd"
        ENROLL.enroll(self.template, [self.pem], first, owner)
        ENROLL.enroll(self.template, [der], second, owner)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        variables = ENROLL.Edk2VarStore(str(second)).get_varlist()
        matches = [
            item
            for item in variables["db"].sigdb
            if item.x509 is not None and item.x509 == self.certificate
        ]
        self.assertEqual(len(matches), 1)
        self.assertEqual(matches[0][0]["guid"], owner)

    def test_invalid_second_certificate_leaves_no_partial_output(self):
        malformed = self.root / "malformed.pem"
        malformed.write_bytes(b"not a certificate")
        output = self.root / "vars.fd"
        with self.assertRaises(ValueError):
            ENROLL.enroll(self.template, [self.pem, malformed], output)
        self.assertFalse(output.exists())

    def test_no_certificates_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "at least one"):
            ENROLL.enroll(self.template, [], self.root / "vars.fd")

    def test_a_private_key_is_not_a_certificate(self):
        private = self.root / "not-a-certificate.pem"
        private.write_bytes(
            self.key.private_bytes(
                serialization.Encoding.PEM,
                serialization.PrivateFormat.PKCS8,
                serialization.NoEncryption(),
            )
        )
        output = self.root / "vars.fd"
        with self.assertRaises(ValueError):
            ENROLL.enroll(self.template, [private], output)
        self.assertFalse(output.exists())

    def test_blank_vars_are_not_a_microsoft_template(self):
        with self.assertRaisesRegex(ValueError, "no enrolled"):
            ENROLL.enroll(
                GUEST / "dist/firmware/x86_64/OVMF_VARS_4M.fd",
                [self.pem],
                self.root / "vars.fd",
            )

    def test_microsoft_template_rebuilds_reproducibly_and_preserves_entries(self):
        template = GUEST / "dist/firmware/x86_64/OVMF_VARS_4M.ms.debian.fd"
        manifest = GUEST / "secureboot/microsoft-2023.json"
        original = template.read_bytes()
        inventory = json.loads((GUEST / "secureboot/debian-inventory.json").read_text())
        self.assertEqual(ENROLL.inspect_store(template), inventory["variables"])
        first, second = self.root / "first.fd", self.root / "second.fd"
        report = ENROLL.enroll(template, [], first, microsoft_manifest=manifest)
        ENROLL.enroll(template, [], second, microsoft_manifest=manifest)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(template.read_bytes(), original)
        before = ENROLL.Edk2VarStore(str(template)).get_varlist()
        after = ENROLL.Edk2VarStore(str(first)).get_varlist()
        self.assertEqual(ENROLL.snapshot(before["PK"]), ENROLL.snapshot(after["PK"]))
        self.assertEqual(before.keys(), after.keys())
        for name in ("KEK", "db", "dbx"):
            self.assertLessEqual(
                ENROLL.signature_entries(before[name]),
                ENROLL.signature_entries(after[name]),
            )
            self.assertEqual(
                ENROLL.snapshot(before[name])[1:], ENROLL.snapshot(after[name])[1:]
            )
        for entry in json.loads(manifest.read_text())["certificates"]:
            fingerprints = {
                item["sha256"]
                for group in report["variables"][entry["variable"]]["lists"]
                for item in group["entries"]
            }
            self.assertIn(entry["sha256"], fingerprints)
        self.assertEqual(sum(len(group) for group in after["dbx"].sigdb), 445)

    def test_microsoft_mode_refuses_a_modified_guest_store(self):
        modified = self.root / "guest.fd"
        ENROLL.enroll(self.template, [self.pem], modified)
        output = self.root / "output.fd"
        with self.assertRaisesRegex(ValueError, "pristine Debian"):
            ENROLL.enroll(
                modified,
                [],
                output,
                microsoft_manifest=GUEST / "secureboot/microsoft-2023.json",
            )
        self.assertFalse(output.exists())

    def test_a_corrupt_pinned_download_is_rejected_without_output(self):
        manifest = json.loads((GUEST / "secureboot/microsoft-2023.json").read_text())
        cache = self.root / "cache"
        cache.mkdir()
        artifact = manifest["license"]
        (cache / artifact["sha256"]).write_bytes(b"corrupt cached download")
        path = self.root / "manifest.json"
        path.write_text(json.dumps(manifest))
        output = self.root / "vars.fd"
        with self.assertRaisesRegex(ValueError, "digest or size mismatch"):
            ENROLL.enroll(
                GUEST / "dist/firmware/x86_64/OVMF_VARS_4M.ms.debian.fd",
                [],
                output,
                microsoft_manifest=path,
                cache=cache,
            )
        self.assertFalse(output.exists())

    def test_certificate_level_dbx_revocations_are_not_a_compatible_default(self):
        manifest = json.loads((GUEST / "secureboot/microsoft-2023.json").read_text())
        cache = self.root / "cache"
        cache.mkdir()
        license_hash = manifest["license"]["sha256"]
        shutil.copyfile(
            GUEST / ".cache/secureboot" / license_hash, cache / license_hash
        )
        manifest["certificates"] = []
        variables = ENROLL.Edk2VarStore(str(self.template)).get_varlist()
        data = variables["PK"].data
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz") as output:
            info = tarfile.TarInfo("dbx.bin")
            info.size = len(data)
            output.addfile(info, io.BytesIO(data))
        digest = hashlib.sha256(archive.getvalue()).hexdigest()
        (cache / digest).write_bytes(archive.getvalue())
        manifest["dbx"].update(
            sha256=digest,
            member="dbx.bin",
            member_sha256=hashlib.sha256(data).hexdigest(),
            entries=1,
        )
        path = self.root / "manifest.json"
        path.write_text(json.dumps(manifest))
        output = self.root / "vars.fd"
        with self.assertRaisesRegex(ValueError, "only SHA256 image revocations"):
            ENROLL.enroll(
                GUEST / "dist/firmware/x86_64/OVMF_VARS_4M.ms.debian.fd",
                [],
                output,
                microsoft_manifest=path,
                cache=cache,
            )
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
