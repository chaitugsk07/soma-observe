-- soma_observe dashboards table.
-- dashboards: user-defined collections of metric panels (config data, not telemetry).
-- panels is opaque jsonb — the frontend owns the panel schema.
-- Not partitioned: small config table.

CREATE TABLE soma_observe.dashboards (
    id              bigint          GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name            text            NOT NULL,
    panels          jsonb           NOT NULL DEFAULT '[]',
    created_at      timestamptz     NOT NULL DEFAULT now(),
    updated_at      timestamptz     NOT NULL DEFAULT now()
);

-- DOWN ==
DROP TABLE IF EXISTS soma_observe.dashboards;
