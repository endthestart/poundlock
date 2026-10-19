use super::*;
use chrono::TimeZone;
use std::io::Read;

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

#[test]
fn keeps_two_weeks_and_each_months_first_for_six_months() {
    let today = day("2026-10-19");
    let dates: Vec<_> = (0..300)
        .map(|n| today - chrono::Duration::days(n))
        .collect();
    let kept = keep(&dates, today);
    let daily: Vec<_> = (0..14).map(|n| today - chrono::Duration::days(n)).collect();
    for date in &daily {
        assert!(kept.contains(date), "{date}");
    }
    for first in [
        "2026-05-01",
        "2026-06-01",
        "2026-07-01",
        "2026-08-01",
        "2026-09-01",
        "2026-10-01",
    ] {
        assert!(kept.contains(&day(first)), "{first}");
    }
    assert!(!kept.contains(&day("2026-04-01")));
    assert!(!kept.contains(&day("2026-10-02")));
    assert_eq!(kept.len(), 14 + 6);
}

#[test]
fn runs_next_at_the_set_time_today_or_tomorrow() {
    let at = NaiveTime::from_hms_opt(8, 30, 0).unwrap();
    let before = Utc.with_ymd_and_hms(2026, 10, 19, 8, 0, 0).unwrap();
    let after = Utc.with_ymd_and_hms(2026, 10, 19, 9, 0, 0).unwrap();
    assert_eq!(
        next_run(before, at),
        Utc.with_ymd_and_hms(2026, 10, 19, 8, 30, 0).unwrap()
    );
    assert_eq!(
        next_run(after, at),
        Utc.with_ymd_and_hms(2026, 10, 20, 8, 30, 0).unwrap()
    );
}

#[test]
fn backs_up_a_live_database_checks_it_and_prunes_old_copies() {
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let database = directory.path().join("poundlock.sqlite3");
    let store = crate::store::Store::open(&database, secrets).unwrap();
    store
        .create_api_token(&crate::domain::CreateApiToken {
            name: "client".into(),
            permissions: vec!["models:read".into()],
        })
        .unwrap();
    let dumps = directory.path().join("dumps");
    std::fs::create_dir_all(&dumps).unwrap();
    // Older copies: nightly since 20 Sep, and three from earlier months.
    let mut old: Vec<String> = ["2026-01-05", "2026-06-01", "2026-06-02"]
        .map(String::from)
        .into();
    for n in 1..=29 {
        old.push((day("2026-10-19") - chrono::Duration::days(n)).to_string());
    }
    for date in &old {
        std::fs::write(dumps.join(format!("poundlock-{date}.sqlite3.gz")), b"old").unwrap();
    }
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 8, 30, 0).unwrap();
    // The store stays open, as the controller's does.
    let report = back_up(&database, &dumps, now).unwrap();
    assert!(report.ok);
    assert_eq!(report.file, "poundlock-2026-10-19.sqlite3.gz");
    assert_eq!(report.integrity, "ok");
    assert_eq!(report.counts["api_tokens"], 1);
    // Kept: the 14 newest (6 to 19 Oct) and each month's first since May.
    let kept: Vec<_> = report.kept.iter().map(|name| &name[10..20]).collect();
    let mut expected = vec!["2026-06-01", "2026-09-20", "2026-10-01"];
    let newest: Vec<String> = (0..14)
        .rev()
        .map(|n| (day("2026-10-19") - chrono::Duration::days(n)).to_string())
        .collect();
    expected.extend(newest.iter().map(String::as_str));
    assert_eq!(kept, expected);
    assert_eq!(report.removed.len(), old.len() + 1 - expected.len());
    assert!(
        report
            .removed
            .contains(&"poundlock-2026-01-05.sqlite3.gz".to_owned())
    );
    assert!(!dumps.join("poundlock-2026-06-02.sqlite3.gz").exists());

    // The copy unzips to a database that opens and holds the token, and its
    // checksum file matches; the master key is not in the folder.
    let packed = std::fs::read(dumps.join(&report.file)).unwrap();
    assert_eq!(hex(&Sha256::digest(&packed)), report.sha256);
    let listed = std::fs::read_to_string(dumps.join(format!("{}.sha256", report.file))).unwrap();
    assert_eq!(listed, format!("{}  {}\n", report.sha256, report.file));
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&packed[..])
        .read_to_end(&mut raw)
        .unwrap();
    let restored = directory.path().join("restored.sqlite3");
    std::fs::write(&restored, raw).unwrap();
    let connection = Connection::open(&restored).unwrap();
    let tokens: i64 = connection
        .query_row("SELECT count(*) FROM api_tokens", [], |row| row.get(0))
        .unwrap();
    assert_eq!(tokens, 1);
    assert!(!dumps.join("master-key").exists());
    let names: Vec<_> = std::fs::read_dir(&dumps)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names
            .iter()
            .any(|name| name.contains(".tmp") || name.contains(".partial")),
        "{names:?}"
    );

    let status: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dumps.join(STATUS_FILE)).unwrap()).unwrap();
    assert_eq!(status["ok"], true);
    assert_eq!(status["file"], "poundlock-2026-10-19.sqlite3.gz");
    drop(store);
}

#[test]
fn a_missing_database_is_a_failed_backup_and_says_so() {
    let directory = tempfile::tempdir().unwrap();
    let dumps = directory.path().join("dumps");
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 8, 30, 0).unwrap();
    assert!(back_up(&directory.path().join("absent.sqlite3"), &dumps, now).is_err());
    let status: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dumps.join(STATUS_FILE)).unwrap()).unwrap();
    assert_eq!(status["ok"], false);
    assert!(status["error"].as_str().unwrap().contains("absent.sqlite3"));
    assert!(!directory.path().join("absent.sqlite3").exists());
}

#[test]
fn a_start_without_a_good_copy_in_the_last_day_backs_up_soon() {
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
    assert!(missed(None, now));
    let good = serde_json::json!({"ok": true, "at": "2026-10-19T08:30:00+00:00"});
    assert!(!missed(Some(&good), now));
    let old = serde_json::json!({"ok": true, "at": "2026-10-17T08:30:00+00:00"});
    assert!(missed(Some(&old), now));
    let failed = serde_json::json!({"ok": false, "at": "2026-10-19T08:30:00+00:00"});
    assert!(missed(Some(&failed), now));
}

#[cfg(unix)]
#[test]
fn a_planted_symlink_or_shared_folder_cannot_redirect_a_backup() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let database = directory.path().join("poundlock.sqlite3");
    let store = crate::store::Store::open(&database, secrets).unwrap();
    let key_before = std::fs::read(directory.path().join("master-key")).unwrap();
    let dumps = directory.path().join("dumps");
    std::fs::create_dir_all(&dumps).unwrap();
    std::fs::set_permissions(&dumps, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Someone who can write the folder points tonight's names at the master key.
    for name in [
        "poundlock-2026-10-19.sqlite3.gz.partial",
        "poundlock-2026-10-19.sqlite3.gz.sha256",
        "last-backup.json.partial",
    ] {
        std::os::unix::fs::symlink(directory.path().join("master-key"), dumps.join(name)).unwrap();
    }
    let now = Utc.with_ymd_and_hms(2026, 10, 19, 8, 30, 0).unwrap();
    let report = back_up(&database, &dumps, now).unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("master-key")).unwrap(),
        key_before
    );
    let gz = std::fs::symlink_metadata(dumps.join(&report.file)).unwrap();
    assert!(gz.is_file());
    assert_eq!(gz.permissions().mode() & 0o777, 0o600);
    // The uncompressed copy never sits in the backup folder.
    assert!(!std::fs::read_dir(&dumps).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));

    // A folder others may write is refused, and so is one that is a symlink.
    std::fs::set_permissions(&dumps, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        format!("{:#}", back_up(&database, &dumps, now).unwrap_err()).contains("written by others")
    );
    std::fs::set_permissions(&dumps, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = directory.path().join("linked");
    std::os::unix::fs::symlink(&dumps, &link).unwrap();
    assert!(format!("{:#}", back_up(&database, &link, now).unwrap_err()).contains("symlink"));
    drop(store);
}

#[test]
fn a_status_that_is_not_a_small_regular_file_is_not_read() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(STATUS_FILE)).unwrap();
    assert!(last_status(directory.path()).is_none());
}
