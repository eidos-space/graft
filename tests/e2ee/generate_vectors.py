#!/usr/bin/env python3
"""Generate reproducible PUBLIC SYNTHETIC fixtures. Never use these keys/nonces."""

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import e2ee_verify as v
from argon2.low_level import Type, hash_secret_raw
from nacl.bindings import crypto_aead_xchacha20poly1305_ietf_encrypt as encrypt

UUID = "00010203-0405-4607-8809-0a0b0c0d0e0f"
ROOT = bytes(range(32))
REPO = v.repository(UUID)
PASSWORD = "synthetic 密码 e\u0301 🦀"


def config():
    salt, nonce, mutation = bytes(range(32, 64)), bytes(range(24)), bytes(range(64, 96))
    prefix = b"GRAFTEC1\x00\x01\x00\x01" + REPO + v.integer(1, 8) + mutation + v.integer(1, 2) + salt
    wrap = hash_secret_raw(PASSWORD.encode(), salt, 3, 65536, 4, 32, Type.ID, 19)
    body = prefix + nonce + encrypt(ROOT, v.encode(b"graft/config-wrap/v1", v.PROFILE, prefix), nonce, wrap)
    data = body + v.mac(v.derive(ROOT, REPO, b"config"), v.encode(b"graft/config-mac/v1", v.PROFILE, body))
    return {"password_utf8": PASSWORD, "password_key_hex": wrap.hex(), "wire_hex": data.hex()}


def object_vector(index, size, plaintext):
    logical = f"store/files/synthetic-{index}"
    key = v.remote_key(ROOT, REPO, "files", logical)
    upload, nonce = v.sha(f"upload-{index}".encode()), v.sha(f"nonce-{index}".encode())[:24]
    count = max(1, (len(plaintext) + size - 1) // size)
    header = v.encode(logical.encode(), b"files", v.integer(len(plaintext), 8), v.sha(plaintext), v.integer(size, 4), v.integer(count, 8), v.integer(0, 4))
    prefix = b"GRAFTEO1\x00\x01\x00\x01" + upload + nonce + v.integer(len(header) + 16, 4)
    aad = v.encode(b"graft/object-header/v1", v.PROFILE, REPO, b"files", key.encode(), prefix)
    encrypted = encrypt(header, aad, nonce, v.upload_key(ROOT, REPO, b"header", key, upload))
    data = prefix + encrypted
    for chunk in range(count):
        aad = v.encode(b"graft/object-chunk/v1", v.PROFILE, REPO, key.encode(), upload, v.sha(encrypted), v.integer(chunk, 8))
        data += encrypt(plaintext[chunk * size:(chunk + 1) * size], aad, b"\x00" * 16 + v.integer(chunk, 8), v.upload_key(ROOT, REPO, b"payload", key, upload))
    return {"logical": logical, "remote": key, "size": size, "plaintext_hex": plaintext.hex(), "wire_hex": data.hex()}


def reference(index, logical, target):
    kind = "head" if logical == "HEAD" else "refs"
    key = v.remote_key(ROOT, REPO, kind, logical)
    if target.startswith("ref: "):
        target_kind, route = 2, bytes.fromhex(v.remote_key(ROOT, REPO, "refs", target[5:]).split("/")[-1])
    elif target.startswith("tag: "):
        target_kind, route = 3, v.route(ROOT, REPO, bytes.fromhex(target.split(" commit: ")[1]))
    else:
        target_kind, route = 1, v.route(ROOT, REPO, bytes.fromhex(target))
    nonce, mutation = v.sha(f"ref-nonce-{index}".encode())[:24], v.sha(f"mutation-{index}".encode())
    prefix = b"GRAFTER1\x00\x01\x00\x01" + bytes([target_kind]) + route + nonce
    aad = v.encode(b"graft/ref/v1", v.PROFILE, REPO, key.encode(), prefix)
    data = prefix + encrypt(v.encode(logical.encode(), target.encode(), mutation), aad, nonce, v.derive(ROOT, REPO, b"ref"))
    return {"logical": logical, "remote": key, "target": target, "wire_hex": data.hex()}


def hint():
    pack, index = "e2ee/packs/" + "a" * 64, "e2ee/indexes/" + "b" * 64
    key = pack.replace("/packs/", "/hints/")
    commits = [{"commit": bytes([i]) * 32, "parents": [bytes([p]) * 32 for p in parents]} for i, parents in [(1, []), (2, [1]), (3, [2, 1])]]
    entries = sorted((v.route(ROOT, REPO, c["commit"]), [v.route(ROOT, REPO, p) for p in c["parents"]]) for c in commits)
    body = v.encode(pack.encode(), index.encode(), v.encode(*(v.encode(c, v.encode(*parents)) for c, parents in entries)))
    prefix = b"GRAFTEH1\x00\x01\x00\x01" + REPO
    tag = v.mac(v.derive(ROOT, REPO, b"hint"), v.encode(b"graft/routing-hint/v1", v.PROFILE, REPO, key.encode(), prefix, body))
    return {"pack": pack, "index": index, "remote": key, "commits": [{"commit": c["commit"].hex(), "parents": [p.hex() for p in c["parents"]]} for c in commits], "wire_hex": (prefix + body + tag).hex()}


def generate():
    objects = [object_vector(i, size, bytes(j % 251 for j in range(length))) for i, (size, length) in enumerate([(16384, 0), (16384, 33), (16384, 16385), (65536, 33), (262144, 33)])]
    refs = [reference(0, "refs/heads/主线", "07" * 32), reference(1, "HEAD", "ref: refs/heads/主线"), reference(2, "refs/tags/v1", "tag: " + "09" * 32 + " commit: " + "07" * 32)]
    routing_hint = hint()
    frames = sorted([(o["remote"].encode(), bytes.fromhex(o["wire_hex"])) for o in objects[:2]] + [(routing_hint["remote"].encode(), bytes.fromhex(routing_hint["wire_hex"]))])
    manifest = v.encode(v.integer(1, 2), b"", b"", v.encode(*(v.encode(key, v.integer(len(data), 8)) for key, data in frames)))
    bundle = b"GRAFTEB1" + v.integer(len(manifest), 4) + manifest + b"".join(v.integer(len(key), 4) + v.integer(len(data), 8) + key + data for key, data in frames)
    return {"generator": "Independent Python framing/HKDF/HMAC; PyNaCl libsodium 1.6.2 bindings; argon2-cffi 25.1.0. PUBLIC SYNTHETIC KEYS ONLY.", "uuid": UUID, "root_hex": ROOT.hex(), "derived_keys": {p: v.derive(ROOT, REPO, p.encode()).hex() for p in ["name", "header", "payload", "ref", "config", "route", "hint"]}, "routing": {"commit_hex": "07" * 32, "route_hex": v.route(ROOT, REPO, bytes([7]) * 32).hex()}, "config": config(), "objects": objects, "refs": refs, "hint": routing_hint, "bundle": {"manifest_hex": manifest.hex(), "wire_hex": bundle.hex()}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    options = parser.parse_args()
    path = Path(__file__).resolve().parents[1] / "fixtures/e2ee/independent-v1.json"
    encoded = json.dumps(generate(), ensure_ascii=False, indent=2) + "\n"
    if options.check:
        v.require(path.read_text() == encoded)
        print("deterministic vectors match")
    else:
        path.write_text(encoded)
        print("wrote synthetic conformance vectors")
