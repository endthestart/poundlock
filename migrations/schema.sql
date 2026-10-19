PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS api_tokens (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  secret_hash TEXT NOT NULL UNIQUE,
  permissions_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_used_at TEXT,
  revoked_at TEXT
);

CREATE TABLE IF NOT EXISTS endpoints (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK(kind IN ('openai', 'litellm')),
  base_url TEXT NOT NULL,
  api_key_ciphertext TEXT NOT NULL,
  created_at TEXT NOT NULL,
  last_checked_at TEXT,
  last_check_ok INTEGER,
  last_check_message TEXT,
  managed_identity_json TEXT NOT NULL DEFAULT '{}',
  revision_nonce TEXT NOT NULL DEFAULT '',
  data_destination TEXT NOT NULL DEFAULT 'unknown'
);

CREATE TABLE IF NOT EXISTS nodes (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  credential_hash TEXT NOT NULL,
  platform TEXT NOT NULL,
  capabilities_json TEXT NOT NULL,
  status TEXT NOT NULL,
  last_seen_at TEXT,
  wol_mac TEXT,
  wol_broadcast TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS enrollment_tokens (
  token_hash TEXT PRIMARY KEY,
  expires_at TEXT NOT NULL,
  consumed_at TEXT
);

CREATE TABLE IF NOT EXISTS model_profiles (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  endpoint_id TEXT NOT NULL REFERENCES endpoints(id),
  model_id TEXT NOT NULL,
  context_tokens INTEGER NOT NULL,
  max_output_tokens INTEGER NOT NULL,
  workload_class TEXT NOT NULL,
  wake_policy TEXT NOT NULL,
  node_id TEXT REFERENCES nodes(id),
  runtime_profile TEXT,
  created_at TEXT NOT NULL,
  last_checked_at TEXT,
  last_check_ok INTEGER,
  last_check_message TEXT,
  aliases_json TEXT NOT NULL DEFAULT '[]',
  idle_seconds INTEGER,
  data_destination TEXT NOT NULL DEFAULT 'unknown',
  destination_from_endpoint INTEGER NOT NULL DEFAULT 0,
  model_label TEXT,
  revision_nonce TEXT NOT NULL DEFAULT '',
  uses_json TEXT NOT NULL DEFAULT '["durable"]'
);

CREATE TABLE IF NOT EXISTS managed_litellm_keys (
  api_token_id TEXT NOT NULL REFERENCES api_tokens(id),
  endpoint_id TEXT NOT NULL REFERENCES endpoints(id),
  virtual_key_ciphertext TEXT NOT NULL,
  virtual_key_hash TEXT NOT NULL,
  remote_key_id TEXT,
  policy_hash TEXT NOT NULL,
  status TEXT NOT NULL,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (api_token_id, endpoint_id)
);

CREATE INDEX IF NOT EXISTS managed_litellm_keys_status
ON managed_litellm_keys(status, updated_at);

CREATE TABLE IF NOT EXISTS harness_profiles (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK(kind IN ('pi')),
  version TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  roots_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  instruction_files_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS jobs (
  id TEXT PRIMARY KEY,
  idempotency_key TEXT NOT NULL UNIQUE,
  request_hash TEXT NOT NULL,
  project_id TEXT REFERENCES projects(id),
  model_profile_id TEXT NOT NULL REFERENCES model_profiles(id),
  harness_profile_id TEXT NOT NULL REFERENCES harness_profiles(id),
  prompt TEXT NOT NULL,
  status TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  cancel_requested INTEGER NOT NULL DEFAULT 0,
  runner_lease_expires_at TEXT,
  wake_last_at TEXT,
  result_json TEXT,
  error_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  api_token_id TEXT REFERENCES api_tokens(id)
);

CREATE INDEX IF NOT EXISTS jobs_status_created
ON jobs(status, created_at);

CREATE TABLE IF NOT EXISTS events (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id TEXT NOT NULL REFERENCES jobs(id),
  kind TEXT NOT NULL,
  message TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS events_job_sequence
ON events(job_id, sequence);

CREATE TABLE IF NOT EXISTS node_actions (
  id TEXT PRIMARY KEY,
  node_id TEXT NOT NULL REFERENCES nodes(id),
  job_id TEXT REFERENCES jobs(id),
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  status TEXT NOT NULL,
  result_json TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS node_actions_pending
ON node_actions(node_id, status, created_at);

CREATE INDEX IF NOT EXISTS node_actions_leases
ON node_actions(node_id, json_extract(payload_json, '$.lease_id'), kind);
