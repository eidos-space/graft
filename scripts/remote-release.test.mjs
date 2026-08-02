import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import test from "node:test";

import {
  archiveName,
  releaseTagForVersion,
  remotePackages,
  resolveReleaseRoot,
  validatePackageMetadata,
} from "./remote-release.mjs";

const repositoryRoot = path.resolve(import.meta.dirname, "..");

test("defines a dependency-safe Remote publish order", () => {
  assert.deepEqual(
    remotePackages.map(({ name }) => name),
    [
      "@eidos.space/graft-remote",
      "@eidos.space/graft-remote-hono",
      "@eidos.space/graft-remote-cloudflare",
    ],
  );
});

test("maps stable and prerelease versions to npm dist-tags", () => {
  assert.equal(releaseTagForVersion("0.1.0"), "latest");
  assert.equal(releaseTagForVersion("0.2.0-rc.1"), "next");
});

test("derives deterministic scoped package archive names", () => {
  assert.equal(
    archiveName("@eidos.space/graft-remote-cloudflare", "0.1.0"),
    "eidos.space-graft-remote-cloudflare-0.1.0.tgz",
  );
});

test("limits destructive preparation to the release-assets directory", () => {
  assert.equal(
    resolveReleaseRoot("release-assets/graft-remote"),
    path.join(repositoryRoot, "release-assets/graft-remote"),
  );
  assert.throws(
    () => resolveReleaseRoot("release-assets"),
    /inside release-assets/,
  );
  assert.throws(() => resolveReleaseRoot("."), /inside release-assets/);
  assert.throws(() => resolveReleaseRoot("/"), /inside release-assets/);
});

test("validates the checked-in Remote package release contract", async () => {
  const metadataByName = new Map();
  for (const releasePackage of remotePackages) {
    const metadata = JSON.parse(
      await fs.readFile(
        path.join(repositoryRoot, releasePackage.directory, "package.json"),
        "utf8",
      ),
    );
    metadataByName.set(metadata.name, metadata);
  }
  const version = metadataByName.get("@eidos.space/graft-remote")?.version;
  assert.equal(typeof version, "string");
  validatePackageMetadata(metadataByName, version);
});

test("rejects a dependency that could escape the release version", () => {
  const metadataByName = new Map(
    remotePackages.map((releasePackage) => [
      releasePackage.name,
      {
        name: releasePackage.name,
        version: "0.1.0",
        dependencies: Object.fromEntries(
          releasePackage.dependencies.map((dependency) => [
            dependency,
            "latest",
          ]),
        ),
        publishConfig: { access: "public", provenance: true },
      },
    ]),
  );
  assert.throws(
    () => validatePackageMetadata(metadataByName, "0.1.0"),
    /workspace:\^/,
  );
});
