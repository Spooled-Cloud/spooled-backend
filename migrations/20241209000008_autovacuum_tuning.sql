-- Autovacuum Tuning for High-Churn Tables
--
-- This migration configures per-table autovacuum settings for tables that
-- experience high write/update rates. These settings help prevent:
-- - Dead tuple accumulation
-- - Table bloat
-- - Query performance degradation

-- Jobs table: Very high churn (status updates, lease renewals)
-- Process 2% of dead tuples (vs default 20%)
ALTER TABLE jobs SET (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_analyze_scale_factor = 0.01,
    autovacuum_vacuum_threshold = 100,
    autovacuum_analyze_threshold = 50
);

-- Job history: Insert-heavy, rarely updated
-- More aggressive analysis, less aggressive vacuum
ALTER TABLE job_history SET (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 500,
    autovacuum_analyze_threshold = 100
);

-- Workers: Frequent heartbeat updates
ALTER TABLE workers SET (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_analyze_scale_factor = 0.01,
    autovacuum_vacuum_threshold = 50,
    autovacuum_analyze_threshold = 25
);

-- Schedules: Moderate updates (last_run_at, next_run_at)
ALTER TABLE schedules SET (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 100,
    autovacuum_analyze_threshold = 50
);

-- Schedule runs: Insert-heavy
ALTER TABLE schedule_runs SET (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_threshold = 100
);

-- Webhook deliveries: Insert-heavy with retries
ALTER TABLE webhook_deliveries SET (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_threshold = 100
);

-- Dead letter queue: Relatively stable
ALTER TABLE dead_letter_queue SET (
    autovacuum_vacuum_scale_factor = 0.1,
    autovacuum_analyze_scale_factor = 0.05,
    autovacuum_vacuum_threshold = 100,
    autovacuum_analyze_threshold = 50
);

-- Queue config: Rarely changes
ALTER TABLE queue_config SET (
    autovacuum_vacuum_scale_factor = 0.2,
    autovacuum_analyze_scale_factor = 0.1
);

-- API keys: Rarely changes (occasional last_used_at updates)
ALTER TABLE api_keys SET (
    autovacuum_vacuum_scale_factor = 0.1,
    autovacuum_analyze_scale_factor = 0.05
);

-- Add comment for documentation
COMMENT ON TABLE jobs IS 'Job queue with aggressive autovacuum for high-churn workloads';
COMMENT ON TABLE workers IS 'Worker registry with aggressive autovacuum for frequent heartbeats';
