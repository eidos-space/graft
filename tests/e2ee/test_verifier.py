#!/usr/bin/env python3
"""Cross-language fixtures and adversarial checks for the independent verifier."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import unicodedata
import unittest
from pathlib import Path
from unittest.mock import patch

WORKSPACE = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(WORKSPACE / "scripts"))
import e2ee_verify as v
from nacl.bindings import crypto_aead_xchacha20poly1305_ietf_encrypt as encrypt

FIXTURE = json.loads((WORKSPACE / "tests/fixtures/e2ee/independent-v1.json").read_text())
NATIVE = None


def wire(entry):
    return bytes.fromhex(entry["wire_hex"])


def verify_fixture(fixture):
    repo, root = v.repository(fixture["uuid"]), bytes.fromhex(fixture["root_hex"])
    for purpose, key in fixture["derived_keys"].items():
        v.require(v.derive(root, repo, purpose.encode()).hex() == key)
    v.require(v.route(root, repo, bytes.fromhex(fixture["routing"]["commit_hex"])).hex() == fixture["routing"]["route_hex"])
    v.require(v.config_root(wire(fixture["config"]), repo, fixture["config"]["password_utf8"].encode()) == root)
    for entry in fixture["objects"]:
        logical, plaintext = v.verify_object(wire(entry), root, repo, entry["remote"])
        v.require(logical == entry["logical"] and plaintext.hex() == entry["plaintext_hex"])
    for entry in fixture["refs"]:
        v.require(v.verify_ref(wire(entry), root, repo, entry["remote"]) == (entry["logical"], entry["target"]))
    hint = fixture["hint"]
    pack, index, topology = v.verify_hint(wire(hint), root, repo, hint["remote"])
    expected = sorted((v.route(root, repo, bytes.fromhex(c["commit"])), [v.route(root, repo, bytes.fromhex(p)) for p in c["parents"]]) for c in hint["commits"])
    v.require((pack, index, topology) == (hint["pack"], hint["index"], expected))
    v.require(v.verify_bundle(wire(fixture["bundle"]), root, repo) == 3)


class VerifierTests(unittest.TestCase):
    def setUp(self):
        self.root = bytes.fromhex(FIXTURE["root_hex"])
        self.repo = v.repository(FIXTURE["uuid"])

    def test_independent_deterministic_vectors(self):
        verify_fixture(FIXTURE)

    def test_randomized_native_envelopes(self):
        if NATIVE is None:
            self.skipTest("native export supplied with --native")
        verify_fixture(json.loads(Path(NATIVE).read_text()))

    def test_config_password_identity_bounds_and_tampering(self):
        data, password = wire(FIXTURE["config"]), FIXTURE["config"]["password_utf8"].encode()
        for invalid in [b"", b"x" * 1025, b"\xff", b"wrong", unicodedata.normalize("NFC", password.decode()).encode()]:
            with self.assertRaises(Exception):
                v.config_root(data, self.repo, invalid)
        for offset in [0, 8, 10, 12, 28, 36, 68, 70, 102, 126, 174, 205]:
            mutated = bytearray(data)
            mutated[offset] ^= 1
            with self.assertRaises(Exception):
                v.config_root(bytes(mutated), self.repo, password)
        for invalid in [data[:-1], data + b"\0", data * 2]:
            with self.assertRaises(Exception):
                v.config_root(invalid, self.repo, password)
        with patch.object(v, "hash_secret_raw", side_effect=AssertionError("KDF must not run")) as kdf:
            with self.assertRaises(ValueError):
                v.config_root(data, b"\0" * 16, password)
            kdf.assert_not_called()

    def test_objects_bind_every_region_chunk_index_root_key_and_repository(self):
        for entry in FIXTURE["objects"]:
            data, key = wire(entry), entry["remote"]
            header_end = 72 + int.from_bytes(data[68:72], "big")
            for offset in sorted({0, 8, 10, 12, 44, 68, 72, header_end - 1, header_end, len(data) - 1}):
                mutated = bytearray(data)
                mutated[offset] ^= 1
                with self.assertRaises(Exception):
                    v.verify_object(bytes(mutated), self.root, self.repo, key)
            for invalid in [b"", data[:71], data[:-1], data + b"\0"]:
                with self.assertRaises(Exception):
                    v.verify_object(invalid, self.root, self.repo, key)
            for root, repo, remote in [(b"\0" * 32, self.repo, key), (self.root, b"\0" * 16, key), (self.root, self.repo, "e2ee/files/" + "0" * 64)]:
                with self.assertRaises(Exception):
                    v.verify_object(data, root, repo, remote)
            if entry["size"] < len(bytes.fromhex(entry["plaintext_hex"])):
                # Two ciphertext chunks cannot be transposed or replaced.
                split = header_end + entry["size"] + 16
                with self.assertRaises(Exception):
                    v.verify_object(data[:header_end] + data[split:] + data[header_end:split], self.root, self.repo, key)

    def test_authenticated_object_header_rejects_invalid_lengths_flags_and_fields(self):
        entry = FIXTURE["objects"][1]
        data, key = wire(entry), entry["remote"]
        length = int.from_bytes(data[68:72], "big")
        aad = v.encode(b"graft/object-header/v1", v.PROFILE, self.repo, b"files", key.encode(), data[:72])
        header_key = v.upload_key(self.root, self.repo, b"header", key, data[12:44])
        decoded = v.decode(v.decrypt(data[72:72 + length], aad, data[44:68], header_key), 7)
        for field, invalid in [(2, v.integer(2**64 - 1, 8)), (3, b""), (4, v.integer(1, 4)), (5, v.integer(2**64 - 1, 8)), (6, v.integer(1, 4)), (0, b"store/files/../unsafe")]:
            fields = decoded.copy()
            fields[field] = invalid
            encoded = v.encode(*fields)
            prefix = data[:68] + v.integer(len(encoded) + 16, 4)
            aad = v.encode(b"graft/object-header/v1", v.PROFILE, self.repo, b"files", key.encode(), prefix)
            encrypted = encrypt(encoded, aad, data[44:68], header_key)
            with self.assertRaises(Exception):
                v.verify_object(prefix + encrypted + data[72 + length:], self.root, self.repo, key)

    def test_ref_and_hint_tampering_truncation_wrong_identity(self):
        for verify, entries in [(v.verify_ref, FIXTURE["refs"]), (v.verify_hint, [FIXTURE["hint"]])]:
            for entry in entries:
                data, key = wire(entry), entry["remote"]
                for offset in range(len(data)):
                    corrupt = bytearray(data)
                    corrupt[offset] ^= 1
                    with self.assertRaises(Exception):
                        verify(bytes(corrupt), self.root, self.repo, key)
                for invalid in [data[:8], data[:-1], data + b"\0"]:
                    with self.assertRaises(Exception):
                        verify(invalid, self.root, self.repo, key)
                with self.assertRaises(Exception):
                    verify(data, b"\0" * 32, self.repo, key)
                with self.assertRaises(Exception):
                    verify(data, self.root, b"\0" * 16, key)

    def test_bundle_framing_exactness_and_authentication(self):
        data = wire(FIXTURE["bundle"])
        for invalid in [b"", data[:-1], data + b"\0", data[:8] + b"\xff" * 4 + data[12:]]:
            with self.assertRaises(Exception):
                v.verify_bundle(invalid, self.root, self.repo)
        for offset in [0, 8, 12, 20, len(data) // 2, len(data) - 1]:
            corrupt = bytearray(data)
            corrupt[offset] ^= 1
            with self.assertRaises(Exception):
                v.verify_bundle(bytes(corrupt), self.root, self.repo)

    def test_codec_exact_widths_lengths_counts_and_ref_names(self):
        for malformed in [b"", b"\0", b"\xff\xff", b"\0\x01\xff\xff\xff\xff", v.encode(b"a") + b"\0"]:
            with self.assertRaises(ValueError):
                v.decode(malformed)
        with self.assertRaises(ValueError):
            v.decode(v.encode(b"a", b"b"), 1)
        with self.assertRaises(ValueError):
            v.number(b"\x01", 8)
        for invalid in ["refs/heads/-x", "refs/heads/a\u2003b", "refs/heads/.x", "refs/heads/@", "refs/remotes/@/a"]:
            with self.assertRaises(ValueError):
                v.ref_name(invalid)
        self.assertEqual(v.ref_name("refs/heads/a./b"), "refs/heads/a./b")
        self.assertEqual(v.ref_name("refs/remotes/.remote/a"), "refs/remotes/.remote/a")

    def test_cli_stdin_and_private_fd_report_no_decrypted_metadata(self):
        entry = FIXTURE["refs"][0]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ref.bin"
            path.write_bytes(wire(entry))
            command = [sys.executable, str(WORKSPACE / "scripts/e2ee_verify.py"), "ref", str(path), "--uuid", FIXTURE["uuid"], "--key", entry["remote"]]
            for root, expected in [(self.root, 0), (b"\0" * 32, 1), (self.root + b"\0", 1)]:
                result = subprocess.run(command, input=root, capture_output=True, check=False)
                self.assertEqual(result.returncode, expected)
                output = result.stdout + result.stderr
                for secret in [self.root.hex().encode(), entry["logical"].encode(), entry["target"].encode()]:
                    self.assertNotIn(secret, output)
            secret_path = Path(directory) / "root"
            secret_path.write_bytes(self.root)
            os.chmod(secret_path, 0o600)
            with secret_path.open("rb") as stream:
                result = subprocess.run(command + ["--secret-fd", str(stream.fileno())], pass_fds=(stream.fileno(),), capture_output=True, check=False)
                self.assertEqual(result.returncode, 0)
            os.chmod(secret_path, 0o644)
            with secret_path.open("rb") as stream:
                result = subprocess.run(command + ["--secret-fd", str(stream.fileno())], pass_fds=(stream.fileno(),), capture_output=True, check=False)
                self.assertEqual(result.returncode, 1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--native")
    options, remaining = parser.parse_known_args()
    NATIVE = options.native
    unittest.main(argv=[sys.argv[0], *remaining])
