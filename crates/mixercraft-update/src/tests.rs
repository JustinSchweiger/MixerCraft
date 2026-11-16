use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;

const TARGET: &str = "windows-x86_64-msi";

fn manifest(version: &str, url: &str, sha: &str, size: u64) -> Manifest {
    Manifest {
        schema: 1,
        version: version.into(),
        published: "2026-11-20".into(),
        notes: "notes".into(),
        notes_url: String::new(),
        assets: vec![Asset { target: TARGET.into(), url: url.into(), sha256: sha.into(), size }],
    }
}

fn sha_of(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mixercraft-update-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Serves `files` (path → body) over HTTP on the loopback interface for as
/// many requests as arrive; returns the base URL.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let response = match files.get(&path) {
                Some(body) => {
                    let mut r =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                            .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    base
}

#[test]
fn newer_version_is_offered_with_resolved_url() {
    let sha = "A".repeat(64);
    let m = manifest("v0.2.0", "MixerCraft-0.2.0-x64.msi", &sha, 10);
    let feed = "https://github.com/o/r/releases/latest/download/latest.json";
    let Check::Available(r) = evaluate(&m, feed, "0.1.0", Some(TARGET)).unwrap() else { panic!("no update") };
    assert_eq!(r.version, Version::new(0, 2, 0));
    let a = r.asset.unwrap();
    assert_eq!(a.url, "https://github.com/o/r/releases/latest/download/MixerCraft-0.2.0-x64.msi");
    assert_eq!(a.sha256, "a".repeat(64));
}

#[test]
fn same_older_and_prerelease_versions_are_not_offered() {
    let m = manifest("0.2.0", "x.msi", &"0".repeat(64), 10);
    for current in ["0.2.0", "0.3.0", "0.2.1-dev"] {
        assert!(matches!(evaluate(&m, "https://h/f.json", current, Some(TARGET)).unwrap(), Check::UpToDate { .. }));
    }
    // A pre-release of the same version is older than the release.
    assert!(matches!(evaluate(&m, "https://h/f.json", "0.2.0-rc.1", Some(TARGET)).unwrap(), Check::Available(_)));
}

#[test]
fn bad_manifests_are_rejected() {
    let feed = "https://h/f.json";
    let mut m = manifest("0.2.0", "x.msi", "abc", 10);
    assert!(matches!(evaluate(&m, feed, "0.1.0", Some(TARGET)), Err(UpdateError::Manifest(_))));
    m.assets[0].sha256 = "g".repeat(64);
    assert!(matches!(evaluate(&m, feed, "0.1.0", Some(TARGET)), Err(UpdateError::Manifest(_))));
    m.assets[0].sha256 = "0".repeat(64);
    m.assets[0].size = 0;
    assert!(matches!(evaluate(&m, feed, "0.1.0", Some(TARGET)), Err(UpdateError::Manifest(_))));
    m.version = "two".into();
    assert!(matches!(evaluate(&m, feed, "0.1.0", Some(TARGET)), Err(UpdateError::Manifest(_))));
    let mut m = manifest("0.2.0", "x.msi", &"0".repeat(64), 10);
    m.schema = 2;
    assert!(matches!(evaluate(&m, feed, "0.1.0", Some(TARGET)), Err(UpdateError::UnsupportedSchema(2))));
}

#[test]
fn release_without_installer_for_this_platform_is_still_reported() {
    let m = manifest("0.2.0", "x.msi", &"0".repeat(64), 10);
    let Check::Available(r) = evaluate(&m, "https://h/f.json", "0.1.0", Some("linux-x86_64-deb")).unwrap() else {
        panic!()
    };
    assert!(r.asset.is_none());
}

#[test]
fn unknown_manifest_fields_are_ignored() {
    let json = r#"{"schema":1,"version":"1.0.0","future":{"x":1},"assets":[]}"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.version, "1.0.0");
}

#[test]
fn urls_resolve_against_the_feed() {
    assert_eq!(resolve_url("https://h/a/latest.json", "f.msi"), "https://h/a/f.msi");
    assert_eq!(resolve_url("https://h/a/latest.json", "./f.msi"), "https://h/a/f.msi");
    assert_eq!(resolve_url("https://h/a/latest.json", "https://cdn/f.msi"), "https://cdn/f.msi");
    assert_eq!(resolve_url("file:///C:/rel/latest.json", "f.msi"), "file:///C:/rel/f.msi");
}

#[test]
fn only_https_file_and_loopback_http_are_allowed() {
    assert!(is_loopback("127.0.0.1:8080/x"));
    assert!(is_loopback("localhost/x"));
    assert!(is_loopback("[::1]:9/x"));
    assert!(!is_loopback("127.0.0.1.evil.example/x"));
    assert!(!is_loopback("localhost@evil.example/x"));
    assert!(!is_loopback("example.org/x"));
    assert!(matches!(check("http://example.org/latest.json", "0.1.0"), Err(UpdateError::InsecureUrl(_))));
    assert!(matches!(check("ftp://example.org/latest.json", "0.1.0"), Err(UpdateError::InsecureUrl(_))));
}

#[test]
fn file_urls_map_to_paths() {
    if cfg!(windows) {
        assert_eq!(file_url_path("/C:/My%20Releases/latest.json"), PathBuf::from("C:/My Releases/latest.json"));
    }
    assert_eq!(file_url_path("/srv/rel/latest.json"), PathBuf::from("/srv/rel/latest.json"));
    assert_eq!(percent_decode("a%2"), "a%2");
}

#[test]
fn download_names_never_come_from_the_url() {
    let a = Asset { target: "../../evil msi".into(), url: "../../x.exe".into(), sha256: String::new(), size: 1 };
    assert_eq!(download_file_name(&Version::new(1, 2, 3), &a), "MixerCraft-1.2.3-______evil_msi.bin");
}

#[test]
fn check_and_download_from_a_fake_http_feed() {
    let installer = b"fake msi contents".repeat(1000);
    let sha = sha_of(&installer);
    let feed =
        serde_json::to_vec(&manifest("9.0.0", "MixerCraft-9.0.0-x64.msi", &sha, installer.len() as u64)).unwrap();
    let mut files = HashMap::new();
    files.insert("/rel/latest.json".to_owned(), feed);
    files.insert("/rel/MixerCraft-9.0.0-x64.msi".to_owned(), installer.clone());
    let base = serve(files);

    let r = match check(&format!("{base}/rel/latest.json"), "0.1.0") {
        Ok(Check::Available(r)) => r,
        other => panic!("{other:?}"),
    };
    let Some(asset) = r.asset.clone().or_else(|| {
        // Other platforms have no target; exercise the download anyway.
        Some(Asset {
            target: TARGET.into(),
            url: format!("{base}/rel/MixerCraft-9.0.0-x64.msi"),
            sha256: sha.clone(),
            size: installer.len() as u64,
        })
    }) else {
        unreachable!()
    };
    let dir = temp_dir("http");
    std::fs::write(dir.join("old-download.msi"), b"old").unwrap();
    let mut last = (0, 0);
    let path = download(&r.version, &asset, &dir, &mut |d, t| last = (d, t)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), installer);
    assert_eq!(last, (installer.len() as u64, installer.len() as u64));
    assert!(!dir.join("old-download.msi").exists(), "old downloads are removed");
    // A second download reuses the verified file.
    assert_eq!(download(&r.version, &asset, &dir, &mut |_, _| {}).unwrap(), path);

    // Missing installer → error; missing feed → "no release yet".
    let missing = Asset { url: format!("{base}/rel/nope.msi"), ..asset.clone() };
    let dir2 = temp_dir("http-missing");
    assert!(matches!(download(&r.version, &missing, &dir2, &mut |_, _| {}), Err(UpdateError::Io { .. })));
    assert!(matches!(check(&format!("{base}/none/latest.json"), "0.1.0"), Err(UpdateError::NoRelease)));
    assert!(matches!(check("file:///no/such/folder/latest.json", "0.1.0"), Err(UpdateError::NoRelease)));
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

#[test]
fn tampered_or_truncated_downloads_are_deleted() {
    let src = temp_dir("src");
    let body = b"installer bytes".to_vec();
    std::fs::write(src.join("a.msi"), &body).unwrap();
    let url = format!("file:///{}", src.join("a.msi").to_string_lossy().replace('\\', "/").trim_start_matches('/'));
    let v = Version::new(1, 0, 0);

    let wrong_sha = Asset { target: TARGET.into(), url: url.clone(), sha256: "0".repeat(64), size: body.len() as u64 };
    let dir = temp_dir("tampered");
    assert!(matches!(download(&v, &wrong_sha, &dir, &mut |_, _| {}), Err(UpdateError::Verification(_))));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing left behind");

    let too_small = Asset { sha256: sha_of(&body), size: body.len() as u64 - 1, ..wrong_sha.clone() };
    assert!(matches!(download(&v, &too_small, &dir, &mut |_, _| {}), Err(UpdateError::TooLarge(_))));
    let too_big = Asset { sha256: sha_of(&body), size: body.len() as u64 + 1, ..wrong_sha.clone() };
    assert!(matches!(download(&v, &too_big, &dir, &mut |_, _| {}), Err(UpdateError::Verification(_))));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    let good = Asset { sha256: sha_of(&body).to_uppercase(), size: body.len() as u64, ..wrong_sha };
    let path = download(&v, &good, &dir, &mut |_, _| {}).unwrap();
    assert!(verify_file(&path, &good.sha256, good.size).is_ok());
    let _ = std::fs::remove_dir_all(src);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn settings_round_trip_and_defaults() {
    let dir = temp_dir("settings");
    let path = dir.join("updates.toml");
    assert_eq!(Settings::load_from(&path), Settings::default());
    assert!(Settings::default().check_automatically);
    let s = Settings { check_automatically: false, feed_url: "file:///C:/rel/latest.json".into() };
    s.save_to(&path).unwrap();
    assert_eq!(Settings::load_from(&path), s);
    assert_eq!(s.feed().as_deref(), Some("file:///C:/rel/latest.json"));
    std::fs::write(&path, "check_automatically = false\n").unwrap();
    assert!(!Settings::load_from(&path).check_automatically);
    std::fs::write(&path, "not toml [").unwrap();
    assert_eq!(Settings::load_from(&path), Settings::default());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn checks_are_due_daily() {
    let day = CHECK_INTERVAL.as_secs();
    assert!(check_due(None, 1000));
    assert!(!check_due(Some(1000), 1000 + day - 1));
    assert!(check_due(Some(1000), 1000 + day));
    assert!(check_due(Some(1000), 10), "clock went backwards");
}

#[test]
fn msi_command_is_passive_and_logged() {
    let cmd = msi_install_command(Path::new(r"C:\Users\A B\x.msi"), Path::new(r"C:\l.log"));
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(args, ["/i", r"C:\Users\A B\x.msi", "/passive", "/norestart", "LAUNCHAPP=1", "/l*v", r"C:\l.log"]);
}
