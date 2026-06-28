-- Initial soma_observe schema.
-- Creates parent partitioned tables only; child partitions are created at
-- runtime by the partition manager (T6) using CREATE TABLE ... PARTITION OF.

-- T1: normalized series + point tables + indexes

CREATE TABLE soma_observe.metric_series (
    series_id   bigint      NOT NULL PRIMARY KEY,
    name        text        NOT NULL,
    resource    jsonb       NOT NULL DEFAULT '{}',
    attributes  jsonb       NOT NULL DEFAULT '{}',
    kind        text        NOT NULL,
    unit        text
);

-- Unique constraint so INSERT ON CONFLICT can resolve the series_id by content.
CREATE UNIQUE INDEX uq_metric_series_key
    ON soma_observe.metric_series (name, kind, resource, attributes);

-- Scalar metric points (gauge, delta sum). Partitioned by ts (monthly by default).
-- Child partitions created at runtime by the partition manager (T6).
CREATE TABLE soma_observe.metric_point (
    series_id   bigint          NOT NULL REFERENCES soma_observe.metric_series (series_id),
    ts          timestamptz     NOT NULL,
    value       double precision NOT NULL,
    PRIMARY KEY (series_id, ts)
) PARTITION BY RANGE (ts);

CREATE INDEX idx_metric_point_ts ON soma_observe.metric_point USING BRIN (ts);

-- Histogram points. Same partition-by-range pattern as metric_point.
-- Child partitions created at runtime by the partition manager (T6). -- T5
CREATE TABLE soma_observe.metric_histogram_point (
    series_id       bigint          NOT NULL REFERENCES soma_observe.metric_series (series_id),
    ts              timestamptz     NOT NULL,
    sum             double precision,
    count           bigint,
    bucket_counts   jsonb,
    bounds          jsonb,
    PRIMARY KEY (series_id, ts)
) PARTITION BY RANGE (ts);

CREATE INDEX idx_metric_histogram_ts ON soma_observe.metric_histogram_point USING BRIN (ts);

-- Log records. Partitioned by ts.
-- Child partitions created at runtime by the partition manager (T6).
-- The PK is (id, ts) because Postgres requires all partitioning columns to be
-- part of the primary key on a partitioned table. Queries that ORDER BY id
-- still get monotonically increasing values within a partition.
CREATE TABLE soma_observe.logs (
    id              bigint          GENERATED ALWAYS AS IDENTITY,
    ts              timestamptz     NOT NULL,
    severity_number int,
    severity_text   text,
    body            text,
    trace_id        text,
    span_id         text,
    resource        jsonb           NOT NULL DEFAULT '{}',
    attributes      jsonb           NOT NULL DEFAULT '{}',
    PRIMARY KEY (id, ts)
) PARTITION BY RANGE (ts);

CREATE INDEX idx_logs_ts ON soma_observe.logs USING BRIN (ts);
CREATE INDEX idx_logs_attributes ON soma_observe.logs USING GIN (attributes);

-- DOWN ==
DROP INDEX IF EXISTS soma_observe.idx_logs_attributes;
DROP INDEX IF EXISTS soma_observe.idx_logs_ts;
DROP TABLE IF EXISTS soma_observe.logs;

DROP INDEX IF EXISTS soma_observe.idx_metric_histogram_ts;
DROP TABLE IF EXISTS soma_observe.metric_histogram_point;

DROP INDEX IF EXISTS soma_observe.idx_metric_point_ts;
DROP TABLE IF EXISTS soma_observe.metric_point;

DROP INDEX IF EXISTS soma_observe.uq_metric_series_key;
DROP TABLE IF EXISTS soma_observe.metric_series;

DROP SCHEMA IF EXISTS soma_observe;
