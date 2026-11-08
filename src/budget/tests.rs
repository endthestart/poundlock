use super::meter::MAX_METERED_BODY;
use super::*;
use crate::domain::{ModelProfile, Pricing};

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn budget(weekly: Option<f64>, monthly: Option<f64>) -> FrontierBudget {
    FrontierBudget {
        weekly_usd: weekly,
        monthly_usd: monthly,
        ..Default::default()
    }
}

pub(crate) fn external_profile() -> ModelProfile {
    ModelProfile {
        id: "frontier".into(),
        name: "frontier".into(),
        aliases: Vec::new(),
        endpoint_id: "gateway".into(),
        model_id: "provider/model".into(),
        model_label: None,
        context_tokens: 200_000,
        max_output_tokens: 32_000,
        uses: std::collections::BTreeSet::from(["interactive".to_string()]),
        workload_class: "interactive-coding".into(),
        wake_policy: "never".into(),
        node_id: None,
        runtime_profile: None,
        idle_seconds: None,
        data_destination: "external".into(),
        data_destination_source: "profile".into(),
        thinking: "model_default".into(),
        thinking_budget_tokens: None,
        capabilities: Vec::new(),
        catalog: Default::default(),
        retirement: Default::default(),
        created_at: "now".into(),
        last_check: Default::default(),
    }
}

#[test]
fn only_external_profiles_are_metered() {
    let mut profile = external_profile();
    assert!(metered(&profile));
    profile.data_destination = "operator_network".into();
    assert!(!metered(&profile));
    profile.data_destination = "unknown".into();
    assert!(!metered(&profile));
}

fn pricing(input: f64, output: f64) -> Pricing {
    Pricing {
        input_per_mtok: input,
        output_per_mtok: output,
        source: "operator".into(),
        refreshed_at: "now".into(),
    }
}

#[test]
fn weeks_start_on_monday_and_months_on_the_first() {
    // Friday 17 October 2026.
    let found = periods(&budget(Some(25.0), Some(100.0)), at("2026-10-16T15:30:00Z"));
    assert_eq!(found[0].name, "weekly");
    assert_eq!(found[0].starts_at, at("2026-10-12T00:00:00Z"));
    assert_eq!(found[0].resets_at, at("2026-10-19T00:00:00Z"));
    assert_eq!(found[1].name, "monthly");
    assert_eq!(found[1].starts_at, at("2026-10-01T00:00:00Z"));
    assert_eq!(found[1].resets_at, at("2026-11-01T00:00:00Z"));
}

#[test]
fn a_monday_starts_its_own_week_and_december_rolls_the_year() {
    let found = periods(&budget(Some(1.0), Some(1.0)), at("2026-12-14T00:00:00Z"));
    assert_eq!(found[0].starts_at, at("2026-12-14T00:00:00Z"));
    assert_eq!(found[1].resets_at, at("2027-01-01T00:00:00Z"));
}

#[test]
fn the_offset_moves_where_periods_begin() {
    // 03:00 UTC on Monday 19 October is still Sunday evening in UTC-5.
    let mut central = budget(Some(1.0), Some(1.0));
    central.utc_offset_minutes = -300;
    let found = periods(&central, at("2026-10-19T03:00:00Z"));
    assert_eq!(found[0].starts_at, at("2026-10-12T05:00:00Z"));
    assert_eq!(found[0].resets_at, at("2026-10-19T05:00:00Z"));
    assert_eq!(found[1].starts_at, at("2026-10-01T05:00:00Z"));
}

#[test]
fn only_configured_periods_are_reported() {
    assert!(periods(&FrontierBudget::default(), Utc::now()).is_empty());
    let monthly = periods(&budget(None, Some(100.0)), Utc::now());
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0].name, "monthly");
}

#[test]
fn validation_refuses_nonsense() {
    assert!(budget(Some(25.0), Some(100.0)).validate().is_ok());
    assert!(budget(Some(0.0), None).validate().is_err());
    assert!(budget(Some(-1.0), None).validate().is_err());
    assert!(budget(None, Some(f64::NAN)).validate().is_err());
    assert!(budget(None, Some(2_000_000.0)).validate().is_err());
    let mut warn = budget(Some(1.0), None);
    warn.warn_at = 1.0;
    assert!(warn.validate().is_err());
    let mut offset = budget(Some(1.0), None);
    offset.utc_offset_minutes = 15 * 60;
    assert!(offset.validate().is_err());
}

#[test]
fn states_follow_committed_spend() {
    let period = periods(&budget(Some(10.0), None), Utc::now()).remove(0);
    let status = |spent, in_flight| PeriodStatus {
        period: period.clone(),
        spent_usd: spent,
        in_flight_usd: in_flight,
    };
    assert_eq!(status(1.0, 0.0).state(0.8), "ok");
    assert_eq!(status(7.0, 1.0).state(0.8), "low");
    assert_eq!(status(9.0, 1.0).state(0.8), "exhausted");
    assert_eq!(status(12.0, 0.0).remaining_usd(), 0.0);
    assert_eq!(status(4.0, 1.5).remaining_usd(), 4.5);
}

#[test]
fn reservations_count_until_they_drop() {
    let gate = BudgetGate::default();
    let first = gate.reserve("a".into(), 0.25);
    let second = gate.reserve("b".into(), 0.5);
    assert_eq!(gate.in_flight_usd(), 0.75);
    drop(first);
    assert_eq!(gate.in_flight_usd(), 0.5);
    drop(second);
    assert_eq!(gate.in_flight_usd(), 0.0);
    assert!(
        gate.in_flight_usd().is_sign_positive(),
        "no -0.0 in reports"
    );
}

#[test]
fn a_failed_check_reserves_nothing_and_sees_what_is_in_flight() {
    let gate = BudgetGate::default();
    let _held = gate.reserve("a".into(), 2.0);
    let refused = gate.try_reserve("b".into(), 1.0, |in_flight| {
        assert_eq!(in_flight, 2.0);
        Err("full")
    });
    assert!(matches!(refused, Err("full")));
    assert_eq!(gate.in_flight_usd(), 2.0);
    let _admitted = gate
        .try_reserve("c".into(), 1.0, |_| Ok::<(), ()>(()))
        .unwrap();
    assert_eq!(gate.in_flight_usd(), 3.0);
}

#[test]
fn estimates_assume_the_whole_output_budget() {
    let mut profile = external_profile();
    assert_eq!(estimate_usd(&profile, &json!({})), None);
    profile.catalog.pricing = Some(pricing(3.0, 15.0));
    profile.max_output_tokens = 1000;
    let body = json!({"max_tokens": 4000});
    let estimate = estimate_usd(&profile, &body).unwrap();
    let input = body.to_string().len() as f64 / 3.0;
    assert!((estimate - (input * 3.0 + 1000.0 * 15.0) / 1e6).abs() < 1e-12);
    let small = estimate_usd(&profile, &json!({"max_tokens": 10})).unwrap();
    assert!(small < estimate);
}

#[test]
fn a_stream_is_metered_from_its_last_usage_event_across_chunk_boundaries() {
    let mut meter = UsageMeter::new(true);
    let events = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30,\"cost\":0.0042}}\n\n",
        "data: [DONE]\n\n"
    );
    for piece in events.as_bytes().chunks(7) {
        meter.observe(piece);
    }
    assert_eq!(
        meter.finish(),
        Some(Usage {
            prompt_tokens: Some(120),
            completion_tokens: Some(30),
            cost_usd: Some(0.0042),
        })
    );
}

#[test]
fn a_stream_without_usage_reports_none_and_a_null_usage_is_ignored() {
    let mut meter = UsageMeter::new(true);
    meter.observe(b"data: {\"choices\":[],\"usage\":null}\n\ndata: [DONE]\n\n");
    assert_eq!(meter.finish(), None);
}

#[test]
fn a_final_event_without_a_newline_is_still_read() {
    let mut meter = UsageMeter::new(true);
    meter.observe(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}");
    assert_eq!(meter.finish().unwrap().prompt_tokens, Some(5));
}

#[test]
fn a_body_is_metered_from_its_usage_and_an_oversized_body_is_not_guessed() {
    let mut meter = UsageMeter::new(false);
    let body = json!({"choices": [], "usage": {"input_tokens": 9, "output_tokens": 3}});
    meter.observe(body.to_string().as_bytes());
    assert_eq!(meter.finish().unwrap().completion_tokens, Some(3));

    let mut huge = UsageMeter::new(false);
    huge.observe(&vec![b' '; MAX_METERED_BODY]);
    huge.observe(br#"{"usage":{"prompt_tokens":1}}"#);
    assert_eq!(huge.finish(), None);
}

#[test]
fn the_charge_prefers_the_most_authoritative_positive_figure() {
    let usage = Usage {
        prompt_tokens: Some(1_000_000),
        completion_tokens: Some(100_000),
        cost_usd: Some(0.5),
    };
    let price = pricing(1.0, 10.0);
    assert_eq!(
        charge(Some(0.75), Some(&usage), Some(&price), Some(9.0)).source,
        "gateway"
    );
    let provider = charge(None, Some(&usage), Some(&price), Some(9.0));
    assert_eq!((provider.cost_usd, provider.source), (0.5, "provider"));
    let priced = charge(
        None,
        Some(&Usage {
            cost_usd: None,
            ..usage.clone()
        }),
        Some(&price),
        Some(9.0),
    );
    assert_eq!((priced.cost_usd, priced.source), (2.0, "pricing"));
    assert_eq!(priced.prompt_tokens, Some(1_000_000));
    // A gateway that says 0 for a streamed answer does not hide its tokens.
    assert_eq!(
        charge(Some(0.0), Some(&usage), Some(&price), None).source,
        "provider"
    );
    let free = charge(Some(0.0), None, None, Some(9.0));
    assert_eq!((free.cost_usd, free.source), (0.0, "gateway"));
    let estimated = charge(None, None, Some(&price), Some(9.0));
    assert_eq!((estimated.cost_usd, estimated.source), (9.0, "estimate"));
    let unpriced = charge(None, None, None, None);
    assert_eq!((unpriced.cost_usd, unpriced.source), (0.0, "unpriced"));
}

#[test]
fn only_streamed_requests_are_asked_for_usage() {
    let mut plain = json!({"model": "x"});
    request_stream_usage(&mut plain);
    assert!(plain.get("stream_options").is_none());
    let mut streamed = json!({"stream": true, "stream_options": {"include_usage": false}});
    request_stream_usage(&mut streamed);
    assert_eq!(streamed["stream_options"]["include_usage"], true);
    let mut odd = json!({"stream": true, "stream_options": "yes"});
    request_stream_usage(&mut odd);
    assert_eq!(odd["stream_options"], json!({"include_usage": true}));
}
