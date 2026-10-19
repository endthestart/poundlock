-- Where prompts go is declared once per model server, and its models inherit
-- it. Gateways keep one declaration per route, on the model.
--
-- No model's effective destination may change. A model server whose live
-- models all declare the same destination now declares it itself, and those
-- models inherit it; their stored value is that same destination. A server
-- whose models disagree stays undeclared and its models keep their own value
-- until the operator declares the server. Nothing is inferred.
ALTER TABLE endpoints ADD COLUMN data_destination TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE model_profiles ADD COLUMN destination_from_endpoint INTEGER NOT NULL DEFAULT 0;
UPDATE endpoints SET data_destination = (
    SELECT MIN(m.data_destination) FROM model_profiles m
    WHERE m.endpoint_id = endpoints.id AND m.removed_at IS NULL
  )
  WHERE kind = 'openai' AND removed_at IS NULL
    AND (SELECT COUNT(DISTINCT m.data_destination) FROM model_profiles m
         WHERE m.endpoint_id = endpoints.id AND m.removed_at IS NULL) = 1;
UPDATE model_profiles SET destination_from_endpoint = 1
  WHERE removed_at IS NULL AND endpoint_id IN (
    SELECT e.id FROM endpoints e
    WHERE e.kind = 'openai' AND e.removed_at IS NULL
      AND (SELECT COUNT(DISTINCT m.data_destination) FROM model_profiles m
           WHERE m.endpoint_id = e.id AND m.removed_at IS NULL) = 1
  );
