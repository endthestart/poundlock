//! The tools' names, descriptions and input schemas, as `tools/list` returns them.

use super::MAX_WAIT_SECONDS;
use serde_json::{Value, json};

/// The tools as MCP `tools/list` describes them.
pub fn definitions() -> Value {
    let job_id =
        json!({"type": "string", "description": "The job's id, as delegate_task returned it."});
    json!([
        {
            "name": "list_models",
            "title": "List Poundlock job models",
            "description": "Lists the models that can run durable jobs on this Poundlock install, with where each sends data, whether it is busy, and measured capabilities. Call it before delegate_task to choose a model.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "delegate_task",
            "title": "Delegate a coding task to a Poundlock job",
            "description": "Uploads a folder and starts a durable job in which a Poundlock worker writes the code for one bounded task. You plan, research, review and test; the job writes code. Name the files to change and the commands that prove the task is done (acceptance_checks); the runner runs them and repairs failures. Freeze tests that define correct behaviour with frozen_paths. Credential files are never uploaded. Use dry_run to see what would be sent. Returns the job id; follow it with wait_for_job.",
            "inputSchema": {
                "type": "object",
                "required": ["directory", "model", "task"],
                "additionalProperties": false,
                "properties": {
                    "directory": {"type": "string", "description": "Absolute path of the folder to upload, inside a folder this server may use."},
                    "model": {"type": "string", "description": "Model name or id from list_models."},
                    "task": {"type": "string", "description": "What to build or change, as a complete, self-contained instruction."},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "Paths, relative to directory, the task should create or change."},
                    "context": {"type": "string", "description": "Decisions, constraints and code to reuse that the worker cannot infer."},
                    "acceptance_checks": {"type": "array", "items": {"type": "string"}, "maxItems": 8, "description": "Shell commands that must succeed afterwards, such as \"cargo test\". Run by the runner in the job's workspace."},
                    "check_timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Seconds each check may take (default 300)."},
                    "frozen_paths": {"type": "array", "items": {"type": "string"}, "description": "Paths the job must not change, such as tests written before the code."},
                    "protect_existing": {"type": "array", "items": {"type": "string"}, "description": "Paths whose existing files must not change; new files beneath them are allowed."},
                    "max_repairs": {"type": "integer", "minimum": 0, "maximum": 3, "description": "Repair rounds after a failed check (default 1 when there are checks)."},
                    "network": {"type": "string", "enum": ["inference_only", "outbound"], "description": "outbound only when the task must download dependencies and the operator allows it."},
                    "dependency_freshness": {"type": "boolean", "description": "Ask the worker to resolve package versions from the registry."},
                    "budget_minutes": {"type": "integer", "minimum": 1, "maximum": 1440, "description": "Minutes the whole job may take, repairs included. A runner with a shorter limit stops it sooner."},
                    "budget_usd": {"type": "number", "minimum": 0.01, "maximum": 1000, "description": "US dollars the job may spend on an external model; its next request after that is refused. Local models cost nothing."},
                    "source": {"type": "string", "enum": ["folder", "git"], "description": "folder (default) uploads the folder. git sends the repository's pushed commit instead, for a repository too large to upload; the folder must be the repository's top folder with everything committed and pushed to an approved HTTPS host."},
                    "idempotency_key": {"type": "string", "description": "Defaults to a hash of the folder, model and task, so repeating an identical call returns the same job."},
                    "dry_run": {"type": "boolean", "description": "List what would be uploaded and the prompt, without starting a job."}
                }
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
        },
        {
            "name": "job_status",
            "title": "Check a Poundlock job",
            "description": "Reports a job's status and its events since a cursor, without waiting.",
            "inputSchema": {
                "type": "object", "required": ["job_id"], "additionalProperties": false,
                "properties": {
                    "job_id": job_id,
                    "events_after": {"type": "integer", "minimum": 0, "description": "Return events after this sequence number (next_cursor from the previous call)."}
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "wait_for_job",
            "title": "Wait for a Poundlock job",
            "description": "Waits until a job finishes or the timeout passes, then reports its status. Jobs often take longer than one wait; call it again while finished is false.",
            "inputSchema": {
                "type": "object", "required": ["job_id"], "additionalProperties": false,
                "properties": {
                    "job_id": job_id,
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT_SECONDS, "description": "Longest to wait (default 240)."}
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "job_result",
            "title": "Read a Poundlock job's result",
            "description": "Summarises a finished job for review: whether its checks passed and their output, frozen paths it touched, which files each patch changes, and the end of the worker's own report. A finished job is not accepted work until you review the patch and its evidence.",
            "inputSchema": {
                "type": "object", "required": ["job_id"], "additionalProperties": false,
                "properties": {"job_id": job_id}
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "get_patch",
            "title": "Get a Poundlock job's patch",
            "description": "Returns a job's patch for review. A patch over 200 KiB must be saved with save_to instead.",
            "inputSchema": {
                "type": "object", "required": ["job_id"], "additionalProperties": false,
                "properties": {
                    "job_id": job_id,
                    "index": {"type": "integer", "minimum": 0, "description": "Patch index (default 0)."},
                    "attempt": {"type": "integer", "minimum": 1, "description": "An earlier attempt's patch; default is the final attempt."},
                    "save_to": {"type": "string", "description": "Absolute path of a file to write the patch to, inside a folder this server may use."}
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "apply_patch",
            "title": "Apply a Poundlock job's patch",
            "description": "Applies a job's patch to a folder, or commits it as a new branch, after checking that it applies cleanly and touches no Git metadata or credential files. Review the patch with get_patch first. check_only tests it without changing anything.",
            "inputSchema": {
                "type": "object", "required": ["job_id", "directory"], "additionalProperties": false,
                "properties": {
                    "job_id": job_id,
                    "directory": {"type": "string", "description": "Absolute path of the folder the job's source came from."},
                    "index": {"type": "integer", "minimum": 0, "description": "Patch index (default 0)."},
                    "attempt": {"type": "integer", "minimum": 1},
                    "check_only": {"type": "boolean", "description": "Only check that the patch applies."},
                    "branch": {"type": "string", "description": "Commit the patch as this new branch on top of the folder's current commit instead of changing the folder. The working tree, index and current branch stay as they are; the commit's author is the job."}
                }
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
        },
        {
            "name": "cancel_job",
            "title": "Cancel a Poundlock job",
            "description": "Asks the controller to cancel a queued or running job.",
            "inputSchema": {
                "type": "object", "required": ["job_id"], "additionalProperties": false,
                "properties": {"job_id": job_id}
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false}
        },
        {
            "name": "provenance",
            "title": "Check that changes came from jobs",
            "description": "Lists the job patches applied to a Git folder through apply_patch, and every file changed some other way since the first: by hand, by a formatter or by anyone else. Use it before calling work done.",
            "inputSchema": {
                "type": "object", "required": ["directory"], "additionalProperties": false,
                "properties": {
                    "directory": {"type": "string", "description": "Absolute path of the folder patches were applied to."}
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }
    ])
}
