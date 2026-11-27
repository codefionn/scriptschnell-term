use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::sync::Arc;

use super::*;

fn key() -> SigningKey {
    SigningKey::from_seed(&[5; 64])
}

fn entry(name: &str, path: &str, body: &[u8]) -> FileEntry {
    let (sha3_256, size) = hash(body, io::sink()).unwrap();
    FileEntry {
        name: name.into(),
        path: path.into(),
        size,
        sha3_256,
    }
}

fn manifest(version: &str, files: Vec<FileEntry>) -> Manifest {
    Manifest {
        version: version.into(),
        platform: "test-platform".into(),
        files,
    }
}

/// Serves `files` (path below the channel URL to body) over plain HTTP.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files = Arc::new(files);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let files = files.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let mut stream = stream;
                match files.get(path.trim_start_matches('/')) {
                    Some(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        );
                    }
                }
            });
        }
    });
    format!("{base}/channel")
}

/// The files of a release source that holds `manifest`, signed by
/// `signer`, and `bodies` (asset name to body), below `channel/`.
fn release_files(
    manifest: &Manifest,
    signer: &SigningKey,
    bodies: &[(&str, &[u8])],
) -> HashMap<String, Vec<u8>> {
    let json = serde_json::to_vec(manifest).unwrap();
    let mut files = HashMap::new();
    files.insert(
        "channel/latest/download/manifest-test-platform.json.sig".to_owned(),
        sign(signer, &json).into_bytes(),
    );
    files.insert(
        "channel/latest/download/manifest-test-platform.json".to_owned(),
        json,
    );
    for (path, body) in bodies {
        files.insert(
            format!("channel/download/v{}/{path}", manifest.version),
            body.to_vec(),
        );
    }
    files
}

/// A channel with one source that holds `manifest`, signed by `signer`,
/// and `bodies`.
fn channel(manifest: &Manifest, signer: &SigningKey, bodies: &[(&str, &[u8])]) -> Channel {
    let source = serve(release_files(manifest, signer, bodies));
    Channel::new(&[source], "test-platform", key().public(), None)
}

#[test]
fn a_signed_newer_release_installs_and_moves_the_old_file_aside() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.exe"), b"old").unwrap();
    let release = manifest(
        "0.2.0",
        vec![
            entry("app.exe", "test-platform-app.exe", b"new app"),
            entry("helper.exe", "test-platform-helper.exe", b"new helper"),
        ],
    );
    let channel = channel(
        &release,
        &key(),
        &[
            ("test-platform-app.exe", b"new app"),
            ("test-platform-helper.exe", b"new helper"),
        ],
    );
    assert_eq!(channel.check("0.2.0").unwrap(), None);
    let found = channel.check("0.1.0").unwrap().unwrap();
    assert_eq!(found, release);
    channel.install(&found, dir.path()).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("app.exe")).unwrap(),
        b"new app"
    );
    assert_eq!(
        std::fs::read(dir.path().join("helper.exe")).unwrap(),
        b"new helper"
    );
    assert_eq!(
        std::fs::read(dir.path().join("app.exe.update-old")).unwrap(),
        b"old"
    );
    remove_leftovers(dir.path());
    assert!(!dir.path().join("app.exe.update-old").exists());
}

#[test]
fn a_file_that_does_not_match_its_hash_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.exe"), b"old").unwrap();
    std::fs::write(dir.path().join("helper.exe"), b"old helper").unwrap();
    let release = manifest(
        "0.2.0",
        vec![
            entry("app.exe", "test-platform-app.exe", b"new app"),
            entry("helper.exe", "test-platform-helper.exe", b"new helper"),
        ],
    );
    let channel = channel(
        &release,
        &key(),
        &[
            ("test-platform-app.exe", b"new app"),
            ("test-platform-helper.exe", b"evil helper"),
        ],
    );
    let error = channel.install(&release, dir.path()).unwrap_err();
    assert!(
        matches!(error, Error::Mismatch(ref name) if name == "helper.exe"),
        "{error}"
    );
    assert_eq!(std::fs::read(dir.path().join("app.exe")).unwrap(), b"old");
    assert_eq!(
        std::fs::read(dir.path().join("helper.exe")).unwrap(),
        b"old helper"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn manifests_need_the_release_key_and_the_right_platform() {
    let release = manifest(
        "0.2.0",
        vec![entry("app.exe", "test-platform-app.exe", b"x")],
    );
    let forged = channel(&release, &SigningKey::from_seed(&[6; 64]), &[]);
    assert!(matches!(forged.latest(), Err(Error::Signature)));

    let mut other = release.clone();
    other.platform = "other".into();
    let misplaced = channel(&other, &key(), &[]);
    assert!(matches!(misplaced.latest(), Err(Error::Platform { .. })));

    let json = serde_json::to_vec(&release).unwrap();
    let signature = sign(&key(), &json);
    let mut tampered = json.clone();
    *tampered.iter_mut().rfind(|b| **b == b'2').unwrap() = b'3';
    assert!(verify(&key().public(), &json, &signature).is_ok());
    assert!(matches!(
        verify(&key().public(), &tampered, &signature),
        Err(Error::Signature)
    ));
}

#[test]
fn entries_stay_inside_the_install_directory_and_the_channel() {
    for (name, path) in [
        ("../app.exe", "0.2.0/app.exe"),
        ("sub\\app.exe", "0.2.0/app.exe"),
        ("C:app.exe", "0.2.0/app.exe"),
        ("..", "0.2.0/app.exe"),
        ("app.exe", "../app.exe"),
        ("app.exe", "/app.exe"),
        ("app.exe", "https://elsewhere/app.exe"),
        ("app.exe", "0.2.0//app.exe"),
    ] {
        let release = manifest("0.2.0", vec![entry(name, path, b"x")]);
        let json = serde_json::to_vec(&release).unwrap();
        let signature = sign(&key(), &json);
        assert!(
            matches!(
                verify(&key().public(), &json, &signature),
                Err(Error::Manifest(_))
            ),
            "{name} {path}"
        );
    }
}

#[test]
fn versions_compare_by_number() {
    assert!(newer("2.1.305", "2.1.304"));
    assert!(newer("0.10.0", "0.9.9"));
    assert!(newer("v1.2", "1.1.9"));
    assert!(newer("1.0.0", "1.0.0-beta.1"));
    assert!(!newer("2.1.303", "2.1.304"));
    assert!(!newer("1.0.0", "1.0.0"));
    assert!(!newer("1.0.0-rc.1", "1.0.0"));
}

#[test]
fn cargo_installs_are_recognized() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("scriptschnell-term");
    std::fs::write(&exe, b"").unwrap();
    assert!(!installed_by_cargo_in(&exe, home.path()), "no record");

    std::fs::write(
        home.path().join(".crates2.json"),
        r#"{"installs":{"cmdterm 0.2.0 (path+file:///src)":{"bins":["scriptschnell-term"]}}}"#,
    )
    .unwrap();
    assert!(installed_by_cargo_in(&exe, home.path()));

    let elsewhere = tempfile::tempdir().unwrap();
    let copy = elsewhere.path().join("scriptschnell-term");
    std::fs::write(&copy, b"").unwrap();
    assert!(
        !installed_by_cargo_in(&copy, home.path()),
        "outside cargo's bin"
    );

    let other = bin.join("scriptschnell-term-old");
    std::fs::write(&other, b"").unwrap();
    assert!(
        !installed_by_cargo_in(&other, home.path()),
        "not in the record"
    );
}

#[test]
fn the_next_source_takes_over_when_the_first_fails() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.exe"), b"old").unwrap();
    let release = manifest(
        "0.3.0",
        vec![entry("app.exe", "test-platform-app.exe", b"new app")],
    );
    // A mirror that lost the file and one that serves a forged manifest.
    let mut lost = release_files(&release, &key(), &[]);
    lost.remove("channel/download/v0.3.0/test-platform-app.exe");
    let lost = serve(lost);
    let forged = serve(release_files(
        &release,
        &SigningKey::from_seed(&[7; 64]),
        &[("test-platform-app.exe", b"evil")],
    ));
    let good = serve(release_files(
        &release,
        &key(),
        &[("test-platform-app.exe", b"new app")],
    ));

    let channel = Channel::new(
        &[forged.clone(), good.clone()],
        "test-platform",
        key().public(),
        None,
    );
    assert_eq!(channel.check("0.2.0").unwrap(), Some(release.clone()));

    let channel = Channel::new(&[lost, good], "test-platform", key().public(), None);
    let found = channel.check("0.2.0").unwrap().unwrap();
    channel.install(&found, dir.path()).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("app.exe")).unwrap(),
        b"new app"
    );

    let only_forged = Channel::new(&[forged], "test-platform", key().public(), None);
    assert!(matches!(only_forged.latest(), Err(Error::Signature)));
}

#[test]
fn the_first_source_that_answers_decides_the_release() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.exe"), b"old").unwrap();
    let old = manifest(
        "0.1.2",
        vec![entry("app.exe", "test-platform-app.exe", b"old app")],
    );
    let new = manifest(
        "0.1.3",
        vec![entry("app.exe", "test-platform-app.exe", b"new app")],
    );
    let stale = serve(release_files(
        &old,
        &key(),
        &[("test-platform-app.exe", b"old app")],
    ));
    let github = serve(release_files(
        &new,
        &key(),
        &[("test-platform-app.exe", b"new app")],
    ));

    // GitHub comes first, so a mirror that lags behind is never asked.
    let channel = Channel::new(&[github, stale], "test-platform", key().public(), None);
    let found = channel.check("0.1.2").unwrap().unwrap();
    assert_eq!(found, new);
    channel.install(&found, dir.path()).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("app.exe")).unwrap(),
        b"new app"
    );
}

#[test]
#[cfg(unix)]
fn an_appimage_release_replaces_the_appimage_and_keeps_it_executable() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let appimage = dir.path().join("my-term.AppImage");
    std::fs::write(&appimage, b"old").unwrap();
    std::fs::set_permissions(&appimage, std::fs::Permissions::from_mode(0o750)).unwrap();
    let release = manifest(
        "0.2.0",
        vec![entry(
            "scriptschnell-term.AppImage",
            "test-platform-scriptschnell-term.AppImage",
            b"new image",
        )],
    );
    let channel = channel(
        &release,
        &key(),
        &[("test-platform-scriptschnell-term.AppImage", b"new image")],
    );
    channel
        .install_to(&release, &Target::AppImage(appimage.clone()))
        .unwrap();
    assert_eq!(std::fs::read(&appimage).unwrap(), b"new image");
    let mode = std::fs::metadata(&appimage).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let two = manifest(
        "0.2.0",
        vec![
            entry("a.AppImage", "a", b"x"),
            entry("b.AppImage", "b", b"x"),
        ],
    );
    assert!(matches!(
        channel.install_appimage(&two, &appimage),
        Err(Error::Manifest(_))
    ));
}

#[test]
fn a_broken_appimage_download_leaves_the_appimage_alone() {
    let dir = tempfile::tempdir().unwrap();
    let appimage = dir.path().join("my-term.AppImage");
    std::fs::write(&appimage, b"old").unwrap();
    let release = manifest(
        "0.2.0",
        vec![entry(
            "x.AppImage",
            "test-platform-x.AppImage",
            b"new image",
        )],
    );
    let channel = channel(
        &release,
        &key(),
        &[("test-platform-x.AppImage", b"bad image")],
    );
    assert!(channel.install_appimage(&release, &appimage).is_err());
    assert_eq!(std::fs::read(&appimage).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn the_installation_decides_how_a_release_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("scriptschnell-term.exe");
    std::fs::write(&exe, b"").unwrap();
    let appimage = dir.path().join("term.AppImage");
    std::fs::write(&appimage, b"").unwrap();
    assert_eq!(
        target_for(&exe, true, None, None),
        Target::Replace(dir.path().to_owned())
    );
    assert_eq!(
        target_for(&exe, false, Some(appimage.clone()), None),
        Target::AppImage(appimage)
    );
    assert_eq!(target_for(&exe, false, None, None), Target::Notify);
    assert_eq!(
        target_for(&exe, false, Some("relative.AppImage".into()), None),
        Target::Notify
    );
    assert_eq!(
        target_for(&exe, false, Some(dir.path().join("gone.AppImage")), None),
        Target::Notify
    );
}

#[test]
fn sources_and_assets_follow_the_github_layout() {
    assert_eq!(
        sources("codefionn/scriptschnell-term"),
        [
            "https://github.com/codefionn/scriptschnell-term/releases",
            "https://releases.codefionn.eu/scriptschnell-term",
        ]
    );
    assert_eq!(
        release_page("codefionn/scriptschnell-term", "0.2.0"),
        "https://github.com/codefionn/scriptschnell-term/releases/tag/v0.2.0"
    );
    assert_eq!(manifest_asset("linux-x86_64"), "manifest-linux-x86_64.json");
    assert_eq!(
        file_asset("windows-x86_64", "scriptschnell-term-sandbox.exe"),
        "windows-x86_64-scriptschnell-term-sandbox.exe"
    );
    assert!(valid_repo("codefionn/scriptschnell-term"));
    for bad in ["", "a", "a/b/c", "../b", "a/..", "a/b c", "a/b?x"] {
        assert!(!valid_repo(bad), "{bad}");
    }
}

#[test]
fn versions_must_be_usable_in_a_tag() {
    for version in ["", "0.2.0/../x", "0.2 0", "0.2.0?"] {
        let release = manifest(version, vec![entry("app.exe", "app.exe", b"x")]);
        let json = serde_json::to_vec(&release).unwrap();
        let signature = sign(&key(), &json);
        assert!(
            matches!(
                verify(&key().public(), &json, &signature),
                Err(Error::Manifest(_))
            ),
            "{version}"
        );
    }
}

/// A PE file with the headers `authenticode` reads and `body` after them.
fn pe(magic: u16, body: &[u8]) -> Vec<u8> {
    let optional = 0x40 + 24;
    let mut file = vec![0u8; optional + 240];
    file[..2].copy_from_slice(b"MZ");
    file[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    file[0x40..0x44].copy_from_slice(b"PE\0\0");
    file[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
    let count = if magic == 0x10b { 92 } else { 108 };
    file[optional + count..optional + count + 4].copy_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(body);
    file
}

/// What a signing tool does to `file`: pads it to 8 bytes, appends a
/// certificate table, points the data directory at it and sets the checksum.
fn sign_pe(file: &[u8]) -> Vec<u8> {
    let optional = 0x40 + 24;
    let directories = if file[optional] == 0x0b && file[optional + 1] == 0x01 {
        96
    } else {
        112
    };
    let entry = optional + directories + 4 * 8;
    let mut signed = file.to_vec();
    signed.resize(signed.len().next_multiple_of(8), 0);
    let offset = signed.len() as u32;
    signed.extend_from_slice(b"\x18\0\0\0\0\x02\x02\0a pkcs7 signature\0\0\0\0\0\0\0");
    let size = signed.len() as u32 - offset;
    signed[entry..entry + 4].copy_from_slice(&offset.to_le_bytes());
    signed[entry + 4..entry + 8].copy_from_slice(&size.to_le_bytes());
    signed[optional + 64..optional + 68].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
    signed
}

#[test]
fn a_signed_copy_is_the_same_program() {
    for magic in [0x10b, 0x20b] {
        let unsigned = pe(magic, b"code\x01\x02\x03");
        authenticode::same_program(&unsigned, &sign_pe(&unsigned)).unwrap();
    }
}

#[test]
fn a_changed_or_extended_program_is_not_the_same() {
    let unsigned = pe(0x20b, b"code\x01\x02\x03");
    let mut changed = unsigned.clone();
    *changed.last_mut().unwrap() = 4;
    let error = authenticode::same_program(&unsigned, &sign_pe(&changed)).unwrap_err();
    assert!(error.contains("different program"), "{error}");

    let mut appended = sign_pe(&unsigned);
    appended.extend_from_slice(b"more");
    let error = authenticode::same_program(&unsigned, &appended).unwrap_err();
    assert!(error.contains("after its signature"), "{error}");
}

#[test]
fn the_signed_file_must_be_signed_and_the_unsigned_one_not() {
    let unsigned = pe(0x20b, b"code");
    let signed = sign_pe(&unsigned);
    let error = authenticode::same_program(&unsigned, &unsigned).unwrap_err();
    assert!(error.contains("not signed"), "{error}");
    let error = authenticode::same_program(&signed, &signed).unwrap_err();
    assert!(error.contains("signed already"), "{error}");
    let error = authenticode::same_program(b"#!/bin/sh", &signed).unwrap_err();
    assert!(error.contains("not a Windows program"), "{error}");
}

/// A type 2 AppImage as far as `appimage` reads it: an ELF header, `body`,
/// zeroed `.sha256_sig` and `.sig_key` sections of 64 and 128 bytes, and
/// the section headers.
fn appimage_file(body: &[u8]) -> Vec<u8> {
    let mut file = vec![0u8; 64];
    file[..4].copy_from_slice(b"\x7fELF");
    file[4] = 2;
    file[5] = 1;
    file[8..11].copy_from_slice(b"AI\x02");
    file.extend_from_slice(body);
    let signature = file.len();
    file.resize(signature + 64, 0);
    let key = file.len();
    file.resize(key + 128, 0);
    let names = file.len();
    file.extend_from_slice(b"\0.sha256_sig\0.sig_key\0.shstrtab\0");
    let names_size = file.len() - names;
    let table = file.len();
    let mut header = |name: u32, offset: usize, size: usize| {
        let mut entry = [0u8; 64];
        entry[..4].copy_from_slice(&name.to_le_bytes());
        entry[24..32].copy_from_slice(&(offset as u64).to_le_bytes());
        entry[32..40].copy_from_slice(&(size as u64).to_le_bytes());
        file.extend_from_slice(&entry);
    };
    header(0, 0, 0);
    header(1, signature, 64);
    header(13, key, 128);
    header(22, names, names_size);
    file[0x28..0x30].copy_from_slice(&(table as u64).to_le_bytes());
    file[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
    file[0x3c..0x3e].copy_from_slice(&4u16.to_le_bytes());
    file[0x3e..0x40].copy_from_slice(&3u16.to_le_bytes());
    file
}

#[test]
fn an_embedded_appimage_signature_reads_back_and_keeps_the_digest() {
    let unsigned = appimage_file(b"runtime and squashfs");
    let digest = appimage::digest(&unsigned).unwrap();
    assert_eq!(digest.len(), 64);
    let mut signed = unsigned.clone();
    appimage::embed(&mut signed, b"-----BEGIN PGP SIGNATURE-----", b"a key").unwrap();
    assert_eq!(appimage::digest(&signed).unwrap(), digest);
    let (signature, key) = appimage::signature(&signed).unwrap();
    assert_eq!(signature, b"-----BEGIN PGP SIGNATURE-----");
    assert_eq!(key, b"a key");
    appimage::same_appimage(&unsigned, &signed).unwrap();

    let error = appimage::embed(&mut signed, b"again", b"a key").unwrap_err();
    assert!(error.contains("signed already"), "{error}");
    let error = appimage::signature(&unsigned).unwrap_err();
    assert!(error.contains("not signed"), "{error}");
}

#[test]
fn an_appimage_signature_must_fit_its_section() {
    let mut file = appimage_file(b"body");
    let error = appimage::embed(&mut file, &[b'a'; 65], b"key").unwrap_err();
    assert!(error.contains("only 64"), "{error}");
    let error = appimage::embed(&mut file, b"sig", b"").unwrap_err();
    assert!(error.contains("key is empty"), "{error}");
}

#[test]
fn a_changed_appimage_is_not_the_same() {
    let unsigned = appimage_file(b"body one");
    let mut signed = appimage_file(b"body two");
    appimage::embed(&mut signed, b"sig", b"key").unwrap();
    let error = appimage::same_appimage(&unsigned, &signed).unwrap_err();
    assert!(error.contains("different AppImage"), "{error}");

    let error = appimage::same_appimage(&unsigned, &unsigned).unwrap_err();
    assert!(error.contains("not signed"), "{error}");
    let error = appimage::same_appimage(&signed, &signed).unwrap_err();
    assert!(error.contains("signed already"), "{error}");
    let error = appimage::sections(b"MZ not an elf").unwrap_err();
    assert!(error.contains("not an ELF"), "{error}");
}
