ALTER TABLE model_profiles ADD COLUMN uses_json TEXT NOT NULL DEFAULT '["durable"]';
UPDATE model_profiles SET uses_json = '["interactive"]' WHERE workload_class = 'interactive-coding';
UPDATE model_profiles SET uses_json = '["durable"]' WHERE workload_class = 'durable-coding-agent';
UPDATE model_profiles SET uses_json = '["completion"]' WHERE workload_class = 'fast-code-completion';
