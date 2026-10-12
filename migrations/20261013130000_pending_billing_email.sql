-- A changed billing email waits in `pending_billing_email` until it is verified.
--
-- `billing_email` doubles as the email-login identity, and changing it used to
-- clear `billing_email_verified_at` at once: a typo, or a new mailbox the owner
-- could not read yet, left email-login users with no working address until
-- they verified the new one from a session they had to keep open.
--
-- Now a change to an org whose current address is verified only fills this
-- column: the verified address keeps resolving email login (and stays the
-- Stripe billing address) while the confirmation code goes to the new one, and
-- verifying moves it into `billing_email` as verified. An org without a
-- verified address is changed directly, as before. Nullable with no default,
-- so adding it rewrites nothing.

SET LOCAL lock_timeout = '5s';

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS pending_billing_email TEXT;
