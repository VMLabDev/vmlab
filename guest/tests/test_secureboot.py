"""Exercise offline enrollment against the shipped Microsoft VARS template."""

import datetime
import importlib.util
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


if __name__ == "__main__":
    unittest.main()
