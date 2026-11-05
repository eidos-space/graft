# Graft Docker 0.2.0

Set up a new server in your browser. Start the container, open its one-time
initialization link and choose local disk or S3-compatible storage. Graft creates
your access key and shows the connection details for Eidos Lite. The setup page
supports English and Simplified Chinese.

- Configure S3 credentials and test bucket reads, writes and deletion in `/setup`.
- Keep initialization state and credentials across restarts in the private data
  volume. Retrieve a lost access key with the server management command.
- Use your existing HTTPS proxy or platform, or add the optional Caddy Compose
  overlay. Caddy is not required.
- Existing deployments using an external key file continue to work and bypass
  web setup. Preserve their Compose files and data when upgrading. New templates
  include `compose.secrets.yaml` for externally managed keys.

The release includes native Linux AMD64 and ARM64 images and digest-pinned
Compose files. Server dependencies remain Remote 0.2.6 and SDK 0.3.39.

Before upgrading, stop the server and back up its data volume and any external
secrets. For S3 installations, back up the matching bucket data too. Switching
storage backends requires a separate deployment and client migration; setup does
not migrate an existing server.
