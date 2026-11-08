//! Following a job: its status, a bounded wait, its result and cancelling it.

use super::*;

impl Tools {
    pub(super) async fn status(&self, args: StatusArgs) -> Result<Outcome> {
        let path = job_path(&args.job_id)?;
        let job = self.controller.get(&path).await?;
        let after = args.events_after.unwrap_or(0).max(0);
        let events = self
            .controller
            .get(&format!("{path}/events?after={after}"))
            .await?;
        let events: Vec<Value> = events
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|event| {
                json!({
                    "sequence": event["sequence"],
                    "kind": event["kind"],
                    "message": tail(event["message"].as_str().unwrap_or_default(), 500),
                })
            })
            .collect();
        let next_cursor = events
            .last()
            .and_then(|event| event["sequence"].as_i64())
            .unwrap_or(after);
        let recent: Vec<Value> = events.iter().rev().take(20).rev().cloned().collect();
        let mut text = format!(
            "Job {} is {}.",
            args.job_id,
            job["status"].as_str().unwrap_or("unknown")
        );
        if let Some(spent) = job["frontier_spend_usd"].as_f64() {
            text.push_str(&format!(" External models: ${spent:.2}"));
            if let Some(cents) = job["policy"]["budget"]["frontier_cents"].as_u64() {
                text.push_str(&format!(" of ${:.2}", cents as f64 / 100.0));
            }
            text.push('.');
        }
        for event in &recent {
            text.push_str(&format!(
                "\n- {}: {}",
                event["kind"].as_str().unwrap_or("event"),
                event["message"].as_str().unwrap_or("")
            ));
        }
        Ok(Outcome::ok(
            text,
            json!({
                "job_id": args.job_id,
                "status": job["status"],
                "finished": finished(&job),
                "error": job["error"],
                "events": recent,
                "events_omitted": events.len().saturating_sub(recent.len()),
                "next_cursor": next_cursor,
                "updated_at": job["updated_at"],
                "frontier_spend_usd": job["frontier_spend_usd"],
                "budget": job["policy"]["budget"],
            }),
        ))
    }

    pub(super) async fn wait(
        &self,
        args: WaitArgs,
        progress: Option<&Progress>,
    ) -> Result<Outcome> {
        let path = job_path(&args.job_id)?;
        let limit = Duration::from_secs(
            args.timeout_seconds
                .unwrap_or(DEFAULT_WAIT_SECONDS)
                .clamp(1, MAX_WAIT_SECONDS),
        );
        let started = tokio::time::Instant::now();
        let deadline = started + limit;
        loop {
            let job = self.controller.get(&path).await?;
            let done = finished(&job);
            if let Some(progress) = progress
                && !done
            {
                let _ = progress.send(format!(
                    "Job {} is {} ({} s waited)",
                    args.job_id,
                    job["status"].as_str().unwrap_or("unknown"),
                    started.elapsed().as_secs()
                ));
            }
            if done || tokio::time::Instant::now() >= deadline {
                let status = job["status"].as_str().unwrap_or("unknown");
                let text = if done {
                    format!(
                        "Job {} finished: {status}. Read job_result before accepting anything.",
                        args.job_id
                    )
                } else {
                    format!(
                        "Job {} is still {status} after {} s. Call wait_for_job again, or job_status for its events.",
                        args.job_id,
                        limit.as_secs()
                    )
                };
                return Ok(Outcome::ok(
                    text,
                    json!({
                        "job_id": args.job_id,
                        "status": job["status"],
                        "finished": done,
                        "error": job["error"],
                    }),
                ));
            }
            tokio::time::sleep(self.poll.min(deadline - tokio::time::Instant::now())).await;
        }
    }

    pub(super) async fn result(&self, id: &str) -> Result<Outcome> {
        let job = self.controller.get(&job_path(id)?).await?;
        let status = job["status"].as_str().unwrap_or("unknown").to_owned();
        if !finished(&job) {
            return Ok(Outcome::ok(
                format!("Job {id} has not finished; it is {status}. Wait for it first."),
                json!({"job_id": id, "status": status, "finished": false}),
            ));
        }
        let result = &job["result"];
        let verification = &result["provenance"]["verification"];
        let checks: Vec<Value> = verification["commands"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|check| {
                json!({
                    "command": check["command"],
                    "exit_code": check["exit_code"],
                    "passed": check["exit_code"] == 0 && check["error"].is_null(),
                    "duration_ms": check["duration_ms"],
                    "error": check["error"],
                    "output_tail": tail(check["output"].as_str().unwrap_or_default(), CHECK_TAIL_CHARS),
                })
            })
            .collect();
        let patches: Vec<Value> = result["patches"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, patch)| {
                let text = patch["patch"].as_str().unwrap_or_default();
                json!({
                    "index": index,
                    "root": patch["root"],
                    "bytes": text.len(),
                    "files": patch_paths(text).unwrap_or_default(),
                    "readable": patch_paths(text).is_ok(),
                })
            })
            .collect();
        let attempts = result["provenance"]["attempt_count"].as_u64().unwrap_or(1);
        let structured = json!({
            "job_id": id,
            "status": status,
            "finished": true,
            "error": job["error"],
            "attempts": attempts,
            "verification": {
                "status": verification["status"],
                "reason": verification["reason"],
                "failure_code": verification["failure_code"],
                "changed_protected_paths": verification["changed_protected_paths"],
                "flagged_paths": verification["flagged_paths"],
                "checks": checks,
            },
            "patches": patches,
            "output_tail": tail(result["output"].as_str().unwrap_or_default(), OUTPUT_TAIL_CHARS),
        });
        let mut text = format!(
            "Job {id} {status} after {attempts} attempt(s). Verification: {}.",
            verification["status"].as_str().unwrap_or("not reported")
        );
        if let Some(message) = job["error"]["message"].as_str() {
            text.push_str(&format!("\nError: {message}"));
        }
        for check in &checks {
            text.push_str(&format!(
                "\n- check `{}`: {}",
                check["command"].as_str().unwrap_or(""),
                if check["passed"] == true {
                    "passed"
                } else {
                    "failed"
                }
            ));
        }
        if let Some(changed) = verification["changed_protected_paths"].as_array()
            && !changed.is_empty()
        {
            text.push_str(&format!(
                "\nIt changed frozen paths: {}",
                changed
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for patch in &patches {
            let files: Vec<&str> = patch["files"]
                .as_array()
                .map(|files| files.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            text.push_str(&format!(
                "\nPatch {}: {} file(s): {}",
                patch["index"],
                files.len(),
                files.join(", ")
            ));
        }
        if patches.is_empty() {
            text.push_str("\nNo patch.");
        }
        text.push_str("\nReview the patch with get_patch before apply_patch.");
        Ok(Outcome::ok(text, structured))
    }

    pub(super) async fn cancel(&self, id: &str) -> Result<Outcome> {
        let job = self
            .controller
            .post(&format!("{}/cancel", job_path(id)?), &json!({}))
            .await?;
        Ok(Outcome::ok(
            format!(
                "Asked Poundlock to cancel job {id}; it is {}.",
                job["status"].as_str().unwrap_or("being cancelled")
            ),
            json!({"job_id": id, "status": job["status"]}),
        ))
    }
}
