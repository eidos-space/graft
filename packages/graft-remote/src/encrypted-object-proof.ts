import { GraftProtocolError, jsonResponse, readLimitedBody } from "./protocol.js";
import type { GraftRepositoryBackend } from "./types.js";

const encoder = new TextEncoder();
const domain = encoder.encode("GRAFT-E2EE-OBJECT-PROOF-V1\0");
const keyPattern = /^e2ee\/(objects|packs|indexes|files|commits|segments)\/[0-9a-f]{64}$/;
const hex = (bytes: Uint8Array) => Array.from(bytes, byte => byte.toString(16).padStart(2, "0")).join("");

export async function encryptedObjectProof(request: Request, backend: GraftRepositoryBackend, path?: string): Promise<Response> {
  const sizeText = request.headers.get("x-graft-object-bytes") ?? "";
  const size = Number(sizeText);
  if (new URL(request.url).pathname.includes("%") || !path || !keyPattern.test(path) || !/^[1-9][0-9]*$/.test(sizeText)
    || !Number.isSafeInteger(size) || size > 1099511627776
    || request.headers.get("content-type") !== "application/octet-stream"
    || request.headers.get("content-length") !== "32") {
    throw new GraftProtocolError(400, "invalid_proof_request", "Invalid integrity challenge");
  }
  const challenge = await readLimitedBody(request, 32);
  if (challenge.length !== 32) throw new GraftProtocolError(400, "invalid_proof_request", "Invalid challenge length");
  if (!backend.encryption || !backend.encryptedObjectProof) {
    throw new GraftProtocolError(501, "object_proof_unavailable", "Integrity challenge unavailable");
  }
  const uuid = backend.encryption.uuid;
  const uuidBytes = Uint8Array.from(uuid.replaceAll("-", "").match(/../g)!, byte => Number.parseInt(byte, 16));
  const key = encoder.encode(path);
  const prefix = new Uint8Array(domain.length + 16 + 4 + key.length + 8 + 32);
  let offset = 0;
  prefix.set(domain, offset); offset += domain.length;
  prefix.set(uuidBytes, offset); offset += 16;
  new DataView(prefix.buffer).setUint32(offset, key.length); offset += 4;
  prefix.set(key, offset); offset += key.length;
  new DataView(prefix.buffer).setBigUint64(offset, BigInt(size)); offset += 8;
  prefix.set(challenge, offset);
  const proof = await backend.encryptedObjectProof(path, prefix);
  if (proof === null) throw new GraftProtocolError(404, "object_not_found", "Object not found");
  if (!Number.isSafeInteger(proof.size) || proof.size < 0 || !/^[0-9a-f]{64}$/.test(proof.sha256)) {
    throw new GraftProtocolError(500, "backend_contract_error", "Invalid integrity challenge result");
  }
  if (proof.size !== size) throw new GraftProtocolError(409, "object_length_mismatch", "Object length changed");
  const response = jsonResponse({ version: 1, uuid, key: path, ciphertext_bytes: size, challenge: hex(challenge), sha256: proof.sha256 });
  response.headers.set("Cache-Control", "no-store");
  return response;
}
