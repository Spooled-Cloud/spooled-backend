-- Outgoing webhooks: four more event types, and a body format.
--
-- Events: job.deadlettered (a job moved to the dead-letter queue), job.retried
-- (a failed, dead-lettered or cancelled job put back to pending by a retry
-- endpoint), workflow.completed and workflow.failed join the list a webhook may
-- subscribe to. The CHECK is replaced, never dropped for good: a webhook still
-- cannot subscribe to an unknown event. The new list is a superset of the old
-- one, so every stored row satisfies it.
--
-- Format: `format` picks the body a delivery is sent as. 'json' (the default,
-- and every existing webhook) is the signed envelope exactly as before; 'slack'
-- sends {"text": ...}, 'discord' {"content": ...} and 'teams' a MessageCard,
-- each built from that envelope at delivery time. ADD COLUMN with a constant
-- default is a catalog change (Postgres 11+), not a table rewrite.
--
-- outgoing_webhooks holds a few rows per organization, so validating both
-- checks under this transaction's lock is a short read.

SET LOCAL lock_timeout = '5s';

ALTER TABLE outgoing_webhooks DROP CONSTRAINT IF EXISTS outgoing_webhook_events_check;
ALTER TABLE outgoing_webhooks
    ADD CONSTRAINT outgoing_webhook_events_check CHECK (
        events <@ ARRAY[
            'job.created',
            'job.started',
            'job.completed',
            'job.failed',
            'job.cancelled',
            'job.deadlettered',
            'job.retried',
            'queue.paused',
            'queue.resumed',
            'worker.registered',
            'worker.deregistered',
            'schedule.triggered',
            'workflow.completed',
            'workflow.failed'
        ]::TEXT[]
    ) NOT VALID;
ALTER TABLE outgoing_webhooks VALIDATE CONSTRAINT outgoing_webhook_events_check;

ALTER TABLE outgoing_webhooks ADD COLUMN IF NOT EXISTS format TEXT NOT NULL DEFAULT 'json';
ALTER TABLE outgoing_webhooks DROP CONSTRAINT IF EXISTS outgoing_webhook_format_check;
ALTER TABLE outgoing_webhooks
    ADD CONSTRAINT outgoing_webhook_format_check
    CHECK (format IN ('json', 'slack', 'discord', 'teams')) NOT VALID;
ALTER TABLE outgoing_webhooks VALIDATE CONSTRAINT outgoing_webhook_format_check;

COMMENT ON COLUMN outgoing_webhooks.format IS
    'Delivery body: json (signed envelope), slack ({"text"}), discord ({"content"}) or teams (MessageCard)';
