use crate::domain::PatchModelProfile;
use crate::server::budget::tests::{ADMIN, configure, priced_answer, spend, state, upstream};
use crate::server::ops_tests::send;
use crate::server::router;
use axum::http::StatusCode;
use serde_json::json;

/// An operator's checks reach the same paid upstream, so a ceiling that missed
/// them would count low.
#[tokio::test]
async fn an_operators_probes_are_charged_and_refused_like_any_other_request() {
    let (_directory, state) = state();
    let (base, _) = upstream(priced_answer).await;
    let (name, _) = configure(&state, &base, "external", Some((1.0, 5.0)));
    let id = state
        .store
        .model_profile_by_name(&name)
        .unwrap()
        .unwrap()
        .id;
    let patch: PatchModelProfile =
        serde_json::from_value(json!({"uses": ["interactive"]})).unwrap();
    state.store.patch_model_profile(&id, &patch).unwrap();
    let app = router(state.clone());
    let verify = format!("/api/v1/model-profiles/{id}/verify-reasoning");
    let test = format!("/api/v1/model-profiles/{id}/test");

    // One probe: 1,000,000 tokens in at $1 and 100,000 out at $5.
    let (status, body) = send(
        app.clone(),
        "POST",
        &verify,
        ADMIN,
        json!({"levels": ["low"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let view = spend(app.clone()).await;
    assert_eq!(view["spend"]["total_usd"], 1.5);
    assert_eq!(view["spend"]["lines"][0]["api_token_id"], "operator");
    assert_eq!(view["spend"]["lines"][0]["estimated_requests"], 0);

    // Test inference hides its usage, so it is charged its estimate.
    let (status, body) = send(app.clone(), "POST", &test, ADMIN, json!({"generate": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let view = spend(app.clone()).await;
    assert!(view["spend"]["total_usd"].as_f64().unwrap() > 1.5, "{view}");
    assert_eq!(view["spend"]["lines"][0]["requests"], 2);
    assert_eq!(view["spend"]["lines"][0]["estimated_requests"], 1);

    // Listing what a server offers costs nothing, so a spent budget allows it.
    let ceiling = json!({"weekly_usd": 1.0});
    let (status, _) = send(
        app.clone(),
        "PUT",
        "/api/v1/frontier-budget",
        ADMIN,
        ceiling,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        app.clone(),
        "POST",
        &test,
        ADMIN,
        json!({"generate": false}),
    )
    .await;
    assert_ne!(status, StatusCode::FORBIDDEN);
    for (uri, body) in [
        (&verify, json!({"levels": ["low"]})),
        (&test, json!({"generate": true})),
    ] {
        let (status, refused) = send(app.clone(), "POST", uri, ADMIN, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert!(
            refused.to_string().contains("frontier_budget_exhausted"),
            "{refused}"
        );
    }
}
