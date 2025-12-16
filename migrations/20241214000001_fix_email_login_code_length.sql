-- Fix email_login_codes code column length to support storing IP addresses for rate limiting
ALTER TABLE email_login_codes ALTER COLUMN code TYPE VARCHAR(255);


