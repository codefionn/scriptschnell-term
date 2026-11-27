## cmdterm-update

scriptschnell-term looks for new releases on GitHub and on a mirror at codefionn.eu.
Windows installs and AppImages install them themselves; other copies tell the user. This
crate checks the releases, verifies what it finds, and swaps the files. `scriptschnell-term-release`
signs the manifests in the release workflows.

### Release layout

Every release is a GitHub release with the tag `v<version>`. Its assets are the files of
every platform, each platform's manifest and that manifest's signature:

```text
https://github.com/codefionn/scriptschnell-term/releases/
  latest/download/manifest-windows-x86_64.json         the newest release's manifest
  latest/download/manifest-windows-x86_64.json.sig     its signature, one line of base64
  latest/download/manifest-linux-x86_64.json(.sig)     the same for the AppImage
  download/v0.2.0/windows-x86_64-scriptschnell-term.exe
  download/v0.2.0/windows-x86_64-scriptschnell-term-sandbox.exe
  download/v0.2.0/linux-x86_64-scriptschnell-term.AppImage
  download/v0.2.0/scriptschnell-term-0.2.0-windows-x86_64-setup.exe   for first installs
```

`https://releases.codefionn.eu/scriptschnell-term/` repeats these paths. The server pulls
the latest GitHub release every few minutes and switches its `latest` only after every
asset arrived (`../infra-infrazeug`, `src/nodes/external/release_mirror.rs`). The app asks
GitHub first, for the manifest and for each file, and the mirror only when GitHub fails.
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
cargo run -p cmdterm-update --bin scriptschnell-term-release -- keygen release.key
```

This writes the secret to `release.key`, the public line to `release.key.pub`, and
prints the public line. In the GitHub repository:

- Variable `SCRIPTSCHNELL_RELEASE_KEY`: the public line.
- Secret `SCRIPTSCHNELL_RELEASE_SIGNING_KEY`: the content of `release.key`.

Keep `release.key` offline afterwards. Anyone with it can ship code to every
installation that updates itself. Changing the key means users must install a build with
the new public key by hand.

### Releasing

A `v<version>` tag runs `.github/workflows/ci.yml`. It builds and tests everything and
creates the GitHub release as an empty draft. The AppImage and the Windows programs stay
unsigned in the run's `linux-x86_64-unsigned` and `windows-x86_64-unsigned` artifacts:
the GPG key and the Certum certificate live on security keys, so CI never holds them.
Then, on a Linux machine with both:

```sh
packaging/linux/sign-release.sh v0.2.0
packaging/windows/sign-release.sh v0.2.0
```

The first script downloads the unsigned AppImage, signs it with GPG and uploads it to the
draft. The second downloads the unsigned programs, signs them, builds the installer from
the signed programs with Inno Setup under Wine, signs the installer and uploads the three
files to the draft. It refuses to start while the draft has no AppImage. It then starts
`.github/workflows/release.yml`, which:

1. checks on Windows that each Windows file has a valid Authenticode signature with a
   timestamp, from a certificate listed in the `WINDOWS_SIGNING_THUMBPRINTS` repository
   variable,
2. checks with `packaging/linux/verify-appimage.sh` that the AppImage's GPG signature
   verifies against `packaging/linux/appimage-signing-key.asc` at the tag,
3. checks with `scriptschnell-term-release check-signed` that each signed file is the one
   ci.yml built with only the signature added (for a program the PE checksum, the
   certificate table entry and the certificate table at the end; for the AppImage its
   `.sha256_sig` and `.sig_key` sections),
4. signs the Linux and Windows manifests over the signed files and publishes the release.

Until then the release is a draft, which neither `releases/latest` nor the mirror sees,
so Linux users get the release only together with Windows users. The uninstaller inside
the installer stays unsigned: Inno Setup signs it only through a sign command it runs
under Wine itself, and osslsigncode is a Linux program.

### Signing the AppImage

`packaging/linux/sign-appimage.sh <AppImage>` signs any AppImage in place, a local
build from `scripts/build-appimage.sh` too. It does what `appimagetool --sign` does at
build time: gpg signs the hex SHA-256 of the AppImage as a detached, armored text
signature, and the script writes the signature into the runtime's `.sha256_sig`
section and the public key into `.sig_key`. Both sections are zeros in an unsigned
AppImage and are zeroed again for the digest, so no other byte changes and the
signature stays valid. AppImageUpdate's `validate` and AppImageLauncher read it there.
The updater does not: it trusts the signed manifest and its SHA3-256 hashes, which cover
the signed AppImage.

The key is `BC070532FE313455FAFDDFA4D826169BC45EC7A9`. gpg signs with its signing
subkey and, with the key on a security key, asks for the PIN and a touch.
`APPIMAGE_SIGNING_KEY` names another key, and `APPIMAGE_SIGNING_KEYFILE` the public key
file the script checks the result against. The public key is
`packaging/linux/appimage-signing-key.asc`; after extending the key's expiry or adding a
subkey, export it again before the next release:

```sh
gpg --armor --export --export-options export-minimal BC070532FE313455FAFDDFA4D826169BC45EC7A9 \
  >packaging/linux/appimage-signing-key.asc
```

To check a downloaded AppImage:

```sh
packaging/linux/verify-appimage.sh linux-x86_64-scriptschnell-term.AppImage
```

It verifies in a temporary keyring, so it neither reads nor changes `~/.gnupg`. Both
scripts build `scriptschnell-term-release` with cargo unless
`SCRIPTSCHNELL_RELEASE_TOOL` names it.

### Signing the Windows files

The script needs `gh` with write access to the repository, `osslsigncode` with libp11
(its PKCS#11 engine), Wine with Inno Setup 6 installed into it, and SimplySign Desktop
logged in, which provides the certificate's key over PKCS#11. Set:

- `CERTUM_PKCS11_MODULE`: the PKCS#11 module SimplySign Desktop installs.
- `CERTUM_KEY`: the key's `pkcs11:` URI. `p11tool --provider "$CERTUM_PKCS11_MODULE"
  --list-all-privkeys --login` lists it.
- `CERTUM_CERTS`: a PEM file with the code signing certificate, followed by the Certum
  intermediate certificate that issued it, so Windows can build the chain offline.

`CERTUM_PIN`, `CERTUM_PKCS11_ENGINE`, `CERTUM_TIMESTAMP_URL` and `ISCC` are optional;
the script's header explains them. Put the certificate's SHA-1 thumbprint into the
`WINDOWS_SIGNING_THUMBPRINTS` repository variable:

```sh
openssl x509 -in certificate.pem -noout -fingerprint -sha1 | sed 's/.*=//; s/://g'
```

A renewed certificate goes into the variable next to the old one, separated by a space,
before the first release signed with it. Signatures carry a timestamp from
`time.certum.pl`, so releases signed before the old certificate expired stay valid.

### Signing by hand

```sh
SCRIPTSCHNELL_RELEASE_SIGNING_KEY="$(cat release.key)" \
  scriptschnell-term-release sign --version 0.2.0 --platform windows-x86_64 --out assets dist/windows-x86_64
scriptschnell-term-release verify --public-key "$(cat release.key.pub)" --platform windows-x86_64 assets
```

`sign` hashes every file in `dist/windows-x86_64/`, copies each into `assets/` under its
asset name and writes `manifest-windows-x86_64.json` and its `.sig` there. Upload
everything in `assets/` to the release.
