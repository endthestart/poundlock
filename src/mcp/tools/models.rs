//! Which models can take a job, and what is left of the frontier budget.

use super::*;

impl Tools {
    pub(super) async fn list_models(&self) -> Result<Outcome> {
        let options = self.options().await?;
        let models: Vec<Value> = options["models"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|model| {
                json!({
                    "name": model["name"],
                    "id": model["id"],
                    "data_destination": model["data_destination"],
                    "availability": model["availability"],
                    "availability_reason": model["availability_reason"],
                    "busy": model["busy"],
                    "context_tokens": model["context_tokens"],
                    "max_output_tokens": model["max_output_tokens"],
                    "capabilities": model["capabilities"].as_array().map(|items| items.iter().map(|item| json!({
                        "name": item["name"], "status": item["status"], "stale": item["stale"],
                    })).collect::<Vec<_>>()),
                })
            })
            .collect();
        let structured = json!({
            "models": models,
            "permissions": options["permissions"],
            "runner_status": options["runner_status"],
            "outbound_network_allowed": options["outbound_network_allowed"],
            "active_jobs_per_token": options["active_jobs_per_token"],
            "policy_limits": options["policy_limits"],
            "source_limits": options["source_limits"],
        });
        let mut text = String::new();
        if models.is_empty() {
            text.push_str("No model on this Poundlock install can run durable jobs.\n");
        } else {
            text.push_str("Models that can run durable jobs:\n");
            for model in &models {
                text.push_str(&format!(
                    "- {} ({}){}{}\n",
                    model["name"].as_str().unwrap_or("?"),
                    model["data_destination"]
                        .as_str()
                        .unwrap_or("unknown destination"),
                    if model["busy"] == true {
                        ", busy now"
                    } else {
                        ""
                    },
                    match model["availability"].as_str() {
                        Some("blocked") => format!(
                            ", blocked: {}",
                            model["availability_reason"].as_str().unwrap_or("unknown")
                        ),
                        _ => String::new(),
                    }
                ));
            }
        }
        if options["runner_status"] != "connected" {
            text.push_str("No job runner has contacted the controller recently; jobs will wait in the queue.\n");
        }
        // The install's ceiling on external spend, from controllers that have
        // one (contract revision 6). Older ones, or a token without
        // models:read, simply say nothing.
        let mut structured = structured;
        if let Ok(usage) = self.controller.get("api/v1/usage").await
            && usage["frontier_budget"]["configured"] == true
        {
            let budget = &usage["frontier_budget"];
            text.push_str(&format!(
                "External models share the install's frontier budget: {}{}.\n",
                budget["remaining_usd"]
                    .as_f64()
                    .map(|left| format!("${left:.2} left"))
                    .unwrap_or_else(|| "standing unknown".into()),
                match budget["state"].as_str() {
                    Some("low") => ", running low",
                    Some("exhausted") => ", used up: external jobs will be refused until it resets",
                    _ => "",
                }
            ));
            structured["frontier_budget"] = budget.clone();
        }
        Ok(Outcome::ok(text.trim_end(), structured))
    }

    pub(super) fn find_model<'a>(&self, options: &'a Value, wanted: &str) -> Result<&'a Value> {
        let models = options["models"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        models
            .iter()
            .find(|model| {
                model["id"] == wanted
                    || model["name"] == wanted
                    || model["aliases"]
                        .as_array()
                        .is_some_and(|aliases| aliases.iter().any(|alias| alias == wanted))
            })
            .with_context(|| {
                format!(
                    "no durable job model is named {wanted}; choose one of: {}",
                    models
                        .iter()
                        .filter_map(|model| model["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}
