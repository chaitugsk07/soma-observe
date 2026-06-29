-- Add exemplar columns to metric_point for cross-signal correlation.
-- Exemplars are optional: a NumberDataPoint may carry a representative
-- trace_id/span_id sampled at the time the measurement was recorded.
-- Only scalar (gauge/sum) points carry exemplars; histogram points do not.

ALTER TABLE soma_observe.metric_point
    ADD COLUMN IF NOT EXISTS exemplar_trace_id text,
    ADD COLUMN IF NOT EXISTS exemplar_span_id  text;

-- DOWN ==
ALTER TABLE soma_observe.metric_point
    DROP COLUMN IF EXISTS exemplar_span_id,
    DROP COLUMN IF EXISTS exemplar_trace_id;
