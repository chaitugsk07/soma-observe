-- soma_observe alerting tables.
-- alert_rules: configuration table for metric-threshold and log-count rules.
-- alert_state: current evaluation state per rule (ok/pending/firing).
-- Neither table is partitioned — they are small config/state tables.

CREATE TABLE soma_observe.alert_rules (
    id              bigint          GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name            text            NOT NULL,
    kind            text            NOT NULL,          -- 'metric' | 'log'
    enabled         boolean         NOT NULL DEFAULT true,
    severity        text            NOT NULL DEFAULT 'warning', -- 'info' | 'warning' | 'critical'
    config          jsonb           NOT NULL,
    for_secs        int             NOT NULL DEFAULT 0,
    webhook_url     text,
    created_at      timestamptz     NOT NULL DEFAULT now(),
    updated_at      timestamptz     NOT NULL DEFAULT now()
);

-- alert_state tracks the live evaluation state for each rule.
-- 1:1 with alert_rules; created lazily on first evaluation.
CREATE TABLE soma_observe.alert_state (
    rule_id         bigint          PRIMARY KEY REFERENCES soma_observe.alert_rules(id) ON DELETE CASCADE,
    state           text            NOT NULL DEFAULT 'ok',  -- 'ok' | 'pending' | 'firing'
    since           timestamptz     NOT NULL DEFAULT now(),
    last_value      double precision,
    last_eval       timestamptz,
    last_notified   timestamptz,
    last_message    text
);

-- DOWN ==
DROP TABLE IF EXISTS soma_observe.alert_state;
DROP TABLE IF EXISTS soma_observe.alert_rules;
