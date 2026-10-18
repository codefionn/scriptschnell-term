## cmdterm-update

Windows builds of scriptschnell-term update themselves from a release channel. This
crate checks the channel, verifies what it finds, and swaps the files. `cmdterm-release`
signs the channel's manifests in the release workflow.

### Channel layout

```text
https://update.scriptschnell.codefionn.eu/scriptschnell-term/
  windows-x86_64/manifest.json        newest release
  windows-x86_64/manifest.json.sig    its signature, one line of base64
  windows-x86_64/0.2.0/scriptschnell-term.exe
  windows-x86_64/0.2.0/cmdterm-sandbox.exe
  linux-x86_64/...                    same, for the AppImage
  downloads/                          the Windows installer for first installs
```

`manifest.json` lists the version, the platform and every file with its size and
SHA3-256 hash:

```json
{
  "version": "0.2.0",
  "platform": "windows-x86_64",
  "files": [
    { "name": "scriptschnell-term.exe", "path": "0.2.0/scriptschnell-term.exe",
      "size": 41234567, "sha3_256": "…" }
  ]
}
```

### What the app checks

1. The signature covers `scriptschnell-term update manifest v1\n` followed by the
   manifest bytes. It is an Ed25519 and an ML-DSA-65 signature, and both must verify
   against the release key built into the app.
2. The manifest's platform must be the app's own, and its version newer than the
   running one. An old signed manifest therefore cannot roll an installation back.
3. File names must be plain names and paths must stay inside the platform directory.
4. Each download must match its size and hash before anything is replaced. Then every
   installed file moves to `<name>.update-old` and the download takes its place. If one
   rename fails, all of them are put back. The next start deletes the `.update-old`
   files, which Windows keeps locked while the old version runs.

The release key comes from `SCRIPTSCHNELL_RELEASE_KEY` at build time. A build without
it, such as a local `cargo build`, never updates itself.
Neither does a build that `cargo install` put into cargo's `bin` directory
(`$CARGO_HOME/bin`, by default `~/.cargo/bin`) and lists in its install record; cargo
updates those.

### Setting up the key

```sh
cargo run -p cmdterm-update --bin cmdterm-release -- keygen release.key
```

This writes the secret to `release.key`, the public line to `release.key.pub`, and
prints the public line. In the GitHub repository:

- Variable `SCRIPTSCHNELL_RELEASE_KEY`: the public line.
- Secret `SCRIPTSCHNELL_RELEASE_SIGNING_KEY`: the content of `release.key`.

Keep `release.key` offline afterwards. Anyone with it can ship code to every Windows
installation. Changing the key means users must install a build with the new public key
by hand.

### Signing by hand

```sh
SCRIPTSCHNELL_RELEASE_SIGNING_KEY="$(cat release.key)" \
  cmdterm-release sign --version 0.2.0 --platform windows-x86_64 dist/windows-x86_64
cmdterm-release verify --public-key "$(cat release.key.pub)" dist/windows-x86_64
```

`sign` hashes every file in `dist/windows-x86_64/0.2.0/` and writes the manifest and
signature into `dist/windows-x86_64/`.
