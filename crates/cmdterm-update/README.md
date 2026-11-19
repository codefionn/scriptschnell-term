## cmdterm-update

scriptschnell-term looks for new releases on GitHub and on a mirror at codefionn.eu.
Windows installs and AppImages install them themselves; other copies tell the user. This
crate checks the releases, verifies what it finds, and swaps the files. `cmdterm-release`
signs the manifests in the release workflow.

### Release layout

Every release is a GitHub release with the tag `v<version>`. Its assets are the files of
every platform, each platform's manifest and that manifest's signature:

```text
https://github.com/codefionn/scriptschnell-term/releases/
  latest/download/manifest-windows-x86_64.json         the newest release's manifest
  latest/download/manifest-windows-x86_64.json.sig     its signature, one line of base64
  latest/download/manifest-linux-x86_64.json(.sig)     the same for the AppImage
  download/v0.2.0/windows-x86_64-scriptschnell-term.exe
  download/v0.2.0/windows-x86_64-cmdterm-sandbox.exe
  download/v0.2.0/linux-x86_64-scriptschnell-term.AppImage
  download/v0.2.0/scriptschnell-term-0.2.0-windows-x86_64-setup.exe   for first installs
```

`https://releases.codefionn.eu/scriptschnell-term/` repeats these paths. The server pulls
the latest GitHub release every few minutes and switches its `latest` only after every
asset arrived (`../infra-infrazeug`, `src/nodes/external/release_mirror.rs`). The app asks
the mirror first and GitHub when the mirror fails, for the manifest and for each file.
Neither source is trusted: only the signature and the hashes are.

The repository comes from `SCRIPTSCHNELL_RELEASE_REPO` at build time (the workflow sets
it to `github.repository`), else `codefionn/scriptschnell-term`. The mirror path is the
repository's name.

A manifest lists the version, the platform and every file with its installed name, its
asset, its size and its SHA3-256 hash:

```json
{
  "version": "0.2.0",
  "platform": "windows-x86_64",
  "files": [
    { "name": "scriptschnell-term.exe", "path": "windows-x86_64-scriptschnell-term.exe",
      "size": 41234567, "sha3_256": "…" }
  ]
}
```

### What the app checks

1. The signature covers `scriptschnell-term update manifest v1\n` followed by the
   manifest bytes. It is an Ed25519 and an ML-DSA-65 signature, and both must verify
   against the release key built into the app. A source whose manifest fails this is
   skipped.
2. The manifest's platform must be the app's own, and its version newer than the
   running one. An old signed manifest therefore cannot roll an installation back.
3. File names must be plain names, assets plain paths, and the version only letters,
   digits and `.+-_`, since it names the tag in download URLs.
4. Each download must match its size and hash before anything is replaced.
   - Windows (`Target::Replace`): every installed file moves to `<name>.update-old` and
     the download takes its place. If one rename fails, all of them are put back. The
     next start deletes the `.update-old` files, which Windows keeps locked while the
     old version runs.
   - AppImage (`Target::AppImage`, found by `$APPIMAGE`): the release has one file. It
     downloads to `.<appimage name>.update-new` next to the AppImage, gets its
     permissions and is renamed over it. The running AppImage keeps the old file open.
   - Anything else (`Target::Notify`): a package manager, `cargo install` or a copied
     binary. The app only shows the release page.

The release key comes from `SCRIPTSCHNELL_RELEASE_KEY` at build time. A build without
it, such as a local `cargo build`, never looks for releases. A build that `cargo install`
put into cargo's `bin` directory (`$CARGO_HOME/bin`, by default `~/.cargo/bin`) and lists
in its install record is left to cargo and only notified.

### Setting up the key

```sh
cargo run -p cmdterm-update --bin cmdterm-release -- keygen release.key
```

This writes the secret to `release.key`, the public line to `release.key.pub`, and
prints the public line. In the GitHub repository:

- Variable `SCRIPTSCHNELL_RELEASE_KEY`: the public line.
- Secret `SCRIPTSCHNELL_RELEASE_SIGNING_KEY`: the content of `release.key`.

Keep `release.key` offline afterwards. Anyone with it can ship code to every
installation that updates itself. Changing the key means users must install a build with
the new public key by hand.

### Signing by hand

```sh
SCRIPTSCHNELL_RELEASE_SIGNING_KEY="$(cat release.key)" \
  cmdterm-release sign --version 0.2.0 --platform windows-x86_64 --out assets dist/windows-x86_64
cmdterm-release verify --public-key "$(cat release.key.pub)" --platform windows-x86_64 assets
```

`sign` hashes every file in `dist/windows-x86_64/`, copies each into `assets/` under its
asset name and writes `manifest-windows-x86_64.json` and its `.sig` there. Upload
everything in `assets/` to the release.
