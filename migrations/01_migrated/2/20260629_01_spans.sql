-- soma_observe spans table.
-- Stores distributed trace spans. Partitioned by start_time (monthly).
-- Child partitions are created at runtime by the partition manager (T6).
--
-- Primary key includes the partition column (start_time) per Postgres requirement.
-- parent_span_id is NULL for root spans.

CREATE TABLE soma_observe.spans (
    trace_id        text            NOT NULL,
    span_id         text            NOT NULL,
    parent_span_id  text,
    name            text            NOT NULL,
    kind            text,
    service_name    text,
    scope_name      text,
    start_time      timestamptz     NOT NULL,
    end_time        timestamptz     NOT NULL,
    duration_ns     bigint          NOT NULL,
    status_code     text,
    status_message  text,
    resource        jsonb           NOT NULL DEFAULT '{}',
    attributes      jsonb           NOT NULL DEFAULT '{}',
    events          jsonb           NOT NULL DEFAULT '[]',
    links           jsonb           NOT NULL DEFAULT '[]',
    PRIMARY KEY (start_time, trace_id, span_id)
) PARTITION BY RANGE (start_time);

CREATE INDEX idx_spans_trace_id ON soma_observe.spans (trace_id, start_time);
CREATE INDEX idx_spans_service  ON soma_observe.spans (service_name, start_time);
CREATE INDEX idx_spans_ts       ON soma_observe.spans USING BRIN (start_time);
CREATE INDEX idx_spans_attrs    ON soma_observe.spans USING GIN (attributes);

-- DOWN ==
DROP INDEX IF EXISTS soma_observe.idx_spans_attrs;
DROP INDEX IF EXISTS soma_observe.idx_spans_ts;
DROP INDEX IF EXISTS soma_observe.idx_spans_service;
DROP INDEX IF EXISTS soma_observe.idx_spans_trace_id;
DROP TABLE IF EXISTS soma_observe.spans;
