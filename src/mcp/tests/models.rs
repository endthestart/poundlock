//! What `list_models` tells an agent about the install's frontier budget.

use super::*;

#[tokio::test]
async fn list_models_says_what_is_left_of_the_frontier_budget() {
    let controller = controller().await;
    let project = project();
    let tools = tools(&controller, project.path());
    let without = tools.call("list_models", json!({})).await;
    assert!(!without.is_error, "{}", without.text);
    assert!(
        !without.text.contains("frontier budget"),
        "{}",
        without.text
    );

    let saved = reqwest::Client::new()
        .put(format!("{}/api/v1/frontier-budget", controller.url))
        .bearer_auth("admin-secret")
        .json(&json!({"weekly_usd": 5}))
        .send()
        .await
        .unwrap();
    assert!(
        saved.status().is_success(),
        "{}",
        saved.text().await.unwrap()
    );

    let with = tools.call("list_models", json!({})).await;
    assert!(
        with.text
            .contains("External models share the install's frontier budget: $5.00 left."),
        "{}",
        with.text
    );
    assert_eq!(with.structured["frontier_budget"]["state"], "ok");
    assert_eq!(with.structured["frontier_budget"]["remaining_usd"], 5.0);
}
