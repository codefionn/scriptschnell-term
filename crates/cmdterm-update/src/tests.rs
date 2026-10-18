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

/// A channel whose platform directory holds `manifest`, signed by `signer`,
/// and `bodies`.
fn channel(manifest: &Manifest, signer: &SigningKey, bodies: &[(&str, &[u8])]) -> Channel {
    let json = serde_json::to_vec(manifest).unwrap();
    let mut files = HashMap::new();
    files.insert(
        "channel/test-platform/manifest.json.sig".to_owned(),
        sign(signer, &json).into_bytes(),
    );
    files.insert("channel/test-platform/manifest.json".to_owned(), json);
    for (path, body) in bodies {
        files.insert(format!("channel/test-platform/{path}"), body.to_vec());
    }
    Channel::new(&serve(files), "test-platform", key().public(), None)
}

#[test]
fn a_signed_newer_release_installs_and_moves_the_old_file_aside() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.exe"), b"old").unwrap();
    let release = manifest(
        "0.2.0",
        vec![
            entry("app.exe", "0.2.0/app.exe", b"new app"),
            entry("helper.exe", "0.2.0/helper.exe", b"new helper"),
        ],
    );
    let channel = channel(
        &release,
        &key(),
        &[
            ("0.2.0/app.exe", b"new app"),
            ("0.2.0/helper.exe", b"new helper"),
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
            entry("app.exe", "0.2.0/app.exe", b"new app"),
            entry("helper.exe", "0.2.0/helper.exe", b"new helper"),
        ],
    );
    let channel = channel(
        &release,
        &key(),
        &[
            ("0.2.0/app.exe", b"new app"),
            ("0.2.0/helper.exe", b"evil helper"),
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
    let release = manifest("0.2.0", vec![entry("app.exe", "0.2.0/app.exe", b"x")]);
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
