use super::*;

fn store() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let secrets =
        crate::crypto::SecretBox::load_or_create(&directory.path().join("master-key")).unwrap();
    let store = Store::open(&directory.path().join("poundlock.sqlite3"), secrets).unwrap();
    (directory, store)
}

fn charge(cost_usd: f64, source: &'static str) -> Charge {
    Charge {
        cost_usd,
        source,
        prompt_tokens: Some(10),
        completion_tokens: Some(2),
    }
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn the_budget_defaults_to_unconfigured_and_round_trips() {
    let (_directory, store) = store();
    assert!(!store.frontier_budget().unwrap().configured());
    let budget = FrontierBudget {
        weekly_usd: Some(25.0),
        monthly_usd: Some(100.0),
        warn_at: 0.9,
        utc_offset_minutes: -300,
    };
    store.save_frontier_budget(&budget).unwrap();
    assert_eq!(store.frontier_budget().unwrap(), budget);
    let invalid = FrontierBudget {
        weekly_usd: Some(-1.0),
        ..Default::default()
    };
    assert!(store.save_frontier_budget(&invalid).is_err());
    assert_eq!(store.frontier_budget().unwrap(), budget);
}

#[test]
fn spend_is_summed_from_a_moment_and_recorded_once_per_request() {
    let (_directory, store) = store();
    for (id, when, cost) in [
        ("a", "2026-10-01T10:00:00Z", 1.0),
        ("b", "2026-10-12T10:00:00Z", 2.0),
        ("c", "2026-10-16T10:00:00Z", 4.0),
    ] {
        let charged = charge(cost, "pricing");
        store
            .record_frontier_spend(&SpendRecord {
                request_id: id,
                api_token_id: "token",
                profile_id: "profile",
                origin: "relay",
                job_id: None,
                charge: &charged,
                at: at(when),
            })
            .unwrap();
    }
    let again = charge(100.0, "gateway");
    store
        .record_frontier_spend(&SpendRecord {
            request_id: "c",
            api_token_id: "token",
            profile_id: "profile",
            origin: "relay",
            job_id: None,
            charge: &again,
            at: at("2026-10-16T11:00:00Z"),
        })
        .unwrap();
    assert_eq!(
        store
            .frontier_spend_since(at("2026-10-12T00:00:00Z"))
            .unwrap(),
        6.0
    );
    assert_eq!(
        store
            .frontier_spend_since(at("2026-10-01T00:00:00Z"))
            .unwrap(),
        7.0
    );
}

#[test]
fn lines_group_by_application_and_profile_and_count_estimates() {
    let (_directory, store) = store();
    for (id, token, cost, source) in [
        ("a", "one", 1.0, "pricing"),
        ("b", "one", 0.5, "estimate"),
        ("c", "two", 3.0, "gateway"),
    ] {
        let charged = charge(cost, source);
        store
            .record_frontier_spend(&SpendRecord {
                request_id: id,
                api_token_id: token,
                profile_id: "profile",
                origin: "job",
                job_id: Some("job"),
                charge: &charged,
                at: Utc::now(),
            })
            .unwrap();
    }
    let lines = store
        .frontier_spend_lines(Utc::now() - chrono::Duration::days(1))
        .unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].api_token_id, "two");
    assert_eq!(lines[1].requests, 2);
    assert_eq!(lines[1].cost_usd, 1.5);
    assert_eq!(lines[1].estimated_requests, 1);
}

#[test]
fn old_spend_is_pruned_when_new_spend_is_recorded() {
    let (_directory, store) = store();
    let charged = charge(1.0, "pricing");
    for (id, when) in [
        ("old", "2025-01-01T00:00:00Z"),
        ("new", "2026-10-16T00:00:00Z"),
    ] {
        store
            .record_frontier_spend(&SpendRecord {
                request_id: id,
                api_token_id: "token",
                profile_id: "profile",
                origin: "relay",
                job_id: None,
                charge: &charged,
                at: at(when),
            })
            .unwrap();
    }
    assert_eq!(
        store
            .frontier_spend_since(at("2000-01-01T00:00:00Z"))
            .unwrap(),
        1.0
    );
}
