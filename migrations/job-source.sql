CREATE TABLE jobs_with_source (
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
  contract_ciphertext TEXT,
  source_ciphertext TEXT
);
INSERT INTO jobs_with_source SELECT id,idempotency_key,request_hash,project_id,
  model_profile_id,harness_profile_id,prompt,status,attempt_id,cancel_requested,
  runner_lease_expires_at,wake_last_at,result_json,error_json,created_at,updated_at,
  contract_ciphertext,source_ciphertext FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_with_source RENAME TO jobs;
CREATE INDEX jobs_status_created ON jobs(status, created_at);
