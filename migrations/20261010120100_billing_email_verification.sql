-- Email login only trusts a billing email the organization has verified.
--
-- `billing_email` doubles as the email-login identity: `POST /auth/email/start`
-- resolves the organization by it. Any key holder could set it to an address
-- they do not own, so the real owner of that address, signing up later, was
-- logged into the squatter's organization instead of getting a new one.
--
-- `billing_email_verified_at` records when the address was proven: by the email
-- signup flow, by the verification code flow, or by an operator through the
-- admin API. Changing the address clears it. Email login, signup and the
-- uniqueness check only consider verified addresses.
--
-- Existing addresses are grandfathered as verified where email login could
-- already resolve them, because un-verifying those would lock current owners
-- out of email login. The old lookup was `billing_email = lower(input)`, so
-- only addresses already stored lower-cased (and trimmed) ever worked; mixed-
-- case rows stay unverified instead of becoming login identities on deploy.
-- Of several live orgs sharing one address, only the oldest is verified.

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS billing_email_verified_at TIMESTAMPTZ;

UPDATE organizations o
SET billing_email_verified_at = COALESCE(o.updated_at, o.created_at, NOW())
WHERE o.billing_email IS NOT NULL
  AND o.billing_email_verified_at IS NULL
  AND o.billing_email = lower(btrim(o.billing_email))
  AND NOT EXISTS (
      SELECT 1
      FROM organizations older
      WHERE older.id <> o.id
        AND older.plan_tier <> 'deleted'
        AND older.billing_email = o.billing_email
        AND (older.created_at, older.id) < (o.created_at, o.id)
  );

CREATE INDEX IF NOT EXISTS idx_organizations_verified_billing_email
    ON organizations (lower(billing_email))
    WHERE billing_email_verified_at IS NOT NULL;
