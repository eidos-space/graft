#!/usr/bin/env python3
"""Independent, bounded encrypted-remote-v1 envelope verifier (not a sync client)."""

import argparse
import hashlib
import hmac
import json
import os
import re
import stat
import sys
import uuid
from pathlib import Path

from argon2.low_level import Type, hash_secret_raw
from nacl.bindings import crypto_aead_xchacha20poly1305_ietf_decrypt as decrypt

PROFILE = b"encrypted-remote-v1"
MAX_BYTES = 64 * 1024 * 1024
MAX_HEADER = 16 * 1024
HEX = re.compile(r"[0-9a-f]{64}\Z")


def require(condition):
    if not condition:
        raise ValueError("invalid encrypted input")


def integer(value, width):
    return value.to_bytes(width, "big")


def number(value, width):
    require(len(value) == width)
    return int.from_bytes(value, "big")


def encode(*parts):
    require(len(parts) <= 65535)
    return integer(len(parts), 2) + b"".join(integer(len(p), 4) + p for p in parts)


def decode(data, count=None, maximum=65535, limit=MAX_HEADER):
    require(2 <= len(data) <= limit)
    size = number(data[:2], 2)
    require(size <= maximum and (count is None or size == count))
    offset, result = 2, []
    for _ in range(size):
        require(offset + 4 <= len(data))
        length = number(data[offset:offset + 4], 4)
        offset += 4
        require(length <= len(data) - offset)
        result.append(data[offset:offset + length])
        offset += length
    require(offset == len(data))
    return result


def sha(value):
    return hashlib.sha256(value).digest()


def mac(key, value):
    return hmac.digest(key, value, "sha256")


def hkdf(secret, salt, info):
    return mac(mac(salt, secret), info + b"\x01")


def repository(value):
    result = uuid.UUID(value).bytes
    require(result[6] >> 4 == 4 and result[8] >> 6 == 2)
    return result


def derive(root, repo, purpose):
    require(len(root) == 32 and len(repo) == 16)
    return hkdf(root, repo, encode(b"graft/e2ee/v1", PROFILE, integer(1, 2), purpose))


def route(root, repo, commit):
    require(len(commit) == 32)
    return mac(derive(root, repo, b"route"), encode(b"graft/route-commit/v1", PROFILE, commit))


def text(value):
    return value.decode("utf-8", "strict")


def logical_key(value):
    require(0 < len(value.encode()) <= 4096)
    require("\\" not in value and all(ord(c) >= 32 and not 127 <= ord(c) <= 159 for c in value))
    require(all(part not in ("", ".", "..") for part in value.split("/")))
    return value


def object_class(logical):
    logical_key(logical)
    if logical.startswith("objects/pack/") and logical.endswith(".pack"):
        return "packs"
    if logical.startswith("objects/pack/") and logical.endswith(".idx"):
        return "indexes"
    if logical.startswith("objects/"):
        return "objects"
    if logical.startswith("store/files/"):
        return "files"
    if logical.startswith("logs/") and len(logical.split("/")) == 4 and logical.split("/")[2] == "commits":
        return "commits"
    require(logical.startswith("segments/"))
    return "segments"


def ref_name(value):
    logical_key(value)
    require(value.startswith(("refs/heads/", "refs/tags/", "refs/remotes/")))
    require(not any(c.isspace() or c in "~^:?*[" for c in value) and ".." not in value and "@{" not in value)
    suffix = value.split("/", 2)[2]
    if value.startswith("refs/remotes/"):
        parts = suffix.split("/", 1)
        require(len(parts) == 2)
        remote, suffix = parts
        require(remote != "@" and not remote.startswith("-") and not remote.endswith((".", ".lock")))
    require(suffix != "@" and not suffix.startswith("-") and not suffix.endswith("."))
    require(all(not p.startswith(".") and not p.endswith(".lock") for p in suffix.split("/")))
    return value


def remote_key(root, repo, kind, logical):
    digest = mac(derive(root, repo, b"name"), encode(b"graft/name/v1", PROFILE, kind.encode(), logical.encode()))
    return f"e2ee/{kind}/{digest.hex()}"


def opaque(key, kinds):
    parts = key.split("/")
    require(len(parts) == 3 and parts[0] == "e2ee" and parts[1] in kinds and HEX.fullmatch(parts[2]))
    return parts[1]


def config_root(data, repo, password):
    require(len(data) == 206 and data[:12] == b"GRAFTEC1\x00\x01\x00\x01")
    require(data[12:28] == repo and number(data[28:36], 8) > 0 and data[68:70] == b"\x00\x01")
    require(1 <= len(password) <= 1024)
    text(password)
    key = hash_secret_raw(password, data[70:102], 3, 65536, 4, 32, Type.ID, 19)
    root = decrypt(data[126:174], encode(b"graft/config-wrap/v1", PROFILE, data[:102]), data[102:126], key)
    require(len(root) == 32)
    expected = mac(derive(root, repo, b"config"), encode(b"graft/config-mac/v1", PROFILE, data[:174]))
    require(hmac.compare_digest(expected, data[174:]))
    return root


def upload_key(root, repo, purpose, key, upload_id):
    return hkdf(derive(root, repo, purpose), upload_id, encode(b"graft/upload-" + purpose + b"/v1", PROFILE, repo, key.encode()))


def verify_object(data, root, repo, key):
    kind = opaque(key, {"objects", "packs", "indexes", "files", "commits", "segments"})
    require(88 <= len(data) <= MAX_BYTES and data[:12] == b"GRAFTEO1\x00\x01\x00\x01")
    length = number(data[68:72], 4)
    require(16 <= length <= MAX_HEADER and 72 + length <= len(data))
    upload_id, nonce = data[12:44], data[44:68]
    encrypted = data[72:72 + length]
    aad = encode(b"graft/object-header/v1", PROFILE, repo, kind.encode(), key.encode(), data[:72])
    header = decrypt(encrypted, aad, nonce, upload_key(root, repo, b"header", key, upload_id))
    logical, claimed_kind, plain_length, digest, chunk_size, count, flags = decode(header, 7)
    logical = text(logical)
    require(object_class(logical) == kind and claimed_kind == kind.encode())
    require(remote_key(root, repo, kind, logical) == key and len(digest) == 32)
    plain_length, chunk_size, count = number(plain_length, 8), number(chunk_size, 4), number(count, 8)
    require(chunk_size in (16384, 65536, 262144) and number(flags, 4) == 0)
    require(count == max(1, (plain_length + chunk_size - 1) // chunk_size))
    require(72 + length + plain_length + count * 16 == len(data))
    offset, remaining, plaintext = 72 + length, plain_length, bytearray()
    payload_key = upload_key(root, repo, b"payload", key, upload_id)
    for index in range(count):
        size = min(chunk_size, remaining)
        aad = encode(b"graft/object-chunk/v1", PROFILE, repo, key.encode(), upload_id, sha(encrypted), integer(index, 8))
        chunk = decrypt(data[offset:offset + size + 16], aad, b"\x00" * 16 + integer(index, 8), payload_key)
        require(len(chunk) == size)
        plaintext.extend(chunk)
        offset += size + 16
        remaining -= size
    require(hmac.compare_digest(sha(plaintext), digest) and offset == len(data))
    return logical, bytes(plaintext)


def verify_ref(data, root, repo, key):
    kind = opaque(key, {"refs", "head"})
    require(85 <= len(data) <= MAX_HEADER and data[:12] == b"GRAFTER1\x00\x01\x00\x01")
    aad = encode(b"graft/ref/v1", PROFILE, repo, key.encode(), data[:69])
    body = decrypt(data[69:], aad, data[45:69], derive(root, repo, b"ref"))
    logical, target, mutation = decode(body, 3)
    logical, target = text(logical), text(target)
    require(len(mutation) == 32)
    require((logical == "HEAD" and kind == "head") or (ref_name(logical) and kind == "refs"))
    require(remote_key(root, repo, kind, logical) == key)
    if target.startswith("ref: "):
        reference = ref_name(target[5:])
        require(logical == "HEAD" and reference.startswith("refs/heads/"))
        expected_kind = 2
        expected_route = bytes.fromhex(remote_key(root, repo, "refs", reference).split("/")[-1])
    elif target.startswith("tag: "):
        require(logical.startswith("refs/tags/"))
        values = target[5:].split(" commit: ")
        require(len(values) == 2 and all(HEX.fullmatch(v) for v in values))
        expected_kind, expected_route = 3, route(root, repo, bytes.fromhex(values[1]))
    else:
        require(HEX.fullmatch(target))
        expected_kind, expected_route = 1, route(root, repo, bytes.fromhex(target))
    require(data[12] == expected_kind and data[13:45] == expected_route)
    return logical, target


def verify_hint(data, root, repo, key):
    opaque(key, {"hints"})
    require(60 <= len(data) <= 1024 * 1024 and data[:12] == b"GRAFTEH1\x00\x01\x00\x01")
    require(data[12:28] == repo)
    aad = encode(b"graft/routing-hint/v1", PROFILE, repo, key.encode(), data[:28], data[28:-32])
    require(hmac.compare_digest(mac(derive(root, repo, b"hint"), aad), data[-32:]))
    pack, index, entries = decode(data[28:-32], 3, limit=1024 * 1024)
    pack, index = text(pack), text(index)
    opaque(pack, {"packs"})
    opaque(index, {"indexes"})
    require(key == pack.replace("/packs/", "/hints/", 1))
    commits, previous = [], None
    for entry in decode(entries, maximum=4096, limit=1024 * 1024):
        commit, parents = decode(entry, 2)
        parents = decode(parents, maximum=32)
        require(len(commit) == 32 and all(len(p) == 32 for p in parents))
        require(len(set(parents)) == len(parents) and (previous is None or previous < commit))
        commits.append((commit, parents))
        previous = commit
    return pack, index, commits


def verify_bundle(data, root, repo):
    require(12 <= len(data) <= MAX_BYTES and data[:8] == b"GRAFTEB1")
    size = number(data[8:12], 4)
    require(size <= 256 * 1024 and 12 + size <= len(data))
    version, target, closure, descriptors = decode(data[12:12 + size], 4, limit=256 * 1024)
    # This tool verifies the read-bundle profile, not fetch/receive closure claims.
    require(version == b"\x00\x01" and target == b"" and closure == b"")
    offset, previous, count = 12 + size, None, 0
    for entry in decode(descriptors, maximum=256, limit=256 * 1024):
        key, length = decode(entry, 2)
        require(len(key) <= 4096)
        key_text, length = text(key), number(length, 8)
        require(previous is None or previous < key)
        previous = key
        require(offset + 12 <= len(data))
        require(number(data[offset:offset + 4], 4) == len(key) and number(data[offset + 4:offset + 12], 8) == length)
        offset += 12
        require(offset + len(key) + length <= len(data) and data[offset:offset + len(key)] == key)
        offset += len(key)
        payload = data[offset:offset + length]
        if key_text.startswith("e2ee/hints/"):
            verify_hint(payload, root, repo, key_text)
        else:
            verify_object(payload, root, repo, key_text)
        offset += length
        count += 1
    require(offset == len(data) and count > 0)
    return count


def read_bounded(path, maximum):
    with open(path, "rb") as stream:
        data = stream.read(maximum + 1)
    require(len(data) <= maximum)
    return data


def read_secret(fd, maximum):
    require(fd >= 0 and not os.isatty(fd))
    metadata = os.fstat(fd)
    if stat.S_ISREG(metadata.st_mode):
        require(metadata.st_mode & 0o077 == 0 and metadata.st_uid == os.getuid())
    with os.fdopen(os.dup(fd), "rb") as stream:
        secret = stream.read(maximum + 1)
    require(len(secret) <= maximum)
    return secret


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("kind", choices=("config", "object", "ref", "hint", "bundle"))
    parser.add_argument("input", type=Path)
    parser.add_argument("--uuid", required=True, help="independently pinned UUIDv4")
    parser.add_argument("--key", help="opaque remote key, required except config/bundle")
    parser.add_argument("--secret-fd", type=int, default=0, help="password bytes for config; raw 32-byte root otherwise; default stdin")
    options = parser.parse_args()
    try:
        repo = repository(options.uuid)
        secret = read_secret(options.secret_fd, 1024 if options.kind == "config" else 32)
        limits = {"config": 206, "ref": MAX_HEADER, "hint": 1024 * 1024}
        data = read_bounded(options.input, limits.get(options.kind, MAX_BYTES))
        if options.kind == "config":
            config_root(data, repo, secret)
        elif options.kind == "bundle":
            require(len(secret) == 32)
            verify_bundle(data, secret, repo)
        else:
            require(len(secret) == 32 and options.key is not None)
            {"object": verify_object, "ref": verify_ref, "hint": verify_hint}[options.kind](data, secret, repo, options.key)
        print(json.dumps({"verified": True, "kind": options.kind}))
        return 0
    except Exception:
        # Never include key material, decrypted names/content, or library error text.
        print('{"verified":false,"error":"verification failed"}', file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
