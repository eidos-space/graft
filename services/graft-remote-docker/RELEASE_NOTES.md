# Graft Docker 0.2.1

The setup page now verifies your private initialization link before showing
storage fields. Opening the server's root address or `/setup` alone gives you
the command and next steps to obtain that link, so you can enter storage details
after authorization is confirmed.

- Copy the `setup-link` command and run it on the server from your Compose
  directory, then open the complete printed URL, including `#token=…`.
- Get specific recovery steps for an invalid link, a different origin, a failed
  verification request or setup that is already complete. Entry guidance is
  available in English and Simplified Chinese.
- Keep authorization guidance when changing language or choosing S3. Failed
  storage checks remain editable, and old setup tabs close their form if another
  tab finishes initialization.

This patch preserves existing server data, access keys and storage configuration.
Before upgrading, stop the server and back up its data volume and external secrets.
For S3 installations, back up the matching bucket data too. For installations
upgraded from 0.1.1 with an external access-key file, keep the existing Compose
file and key mount; they continue to bypass web setup. Restarting or upgrading
does not reopen setup. Changing storage backends still requires a separate
deployment and client migration.

The release includes Linux AMD64 and ARM64 images and digest-pinned Compose
files. Server dependencies remain Remote 0.2.6 and SDK 0.3.39.
