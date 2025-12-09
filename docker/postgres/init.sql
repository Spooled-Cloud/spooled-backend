-- PostgreSQL Initialization Script for Spooled Backend
--
-- This script runs on first database initialization.
-- For schema migrations, use sqlx-cli migrations.

-- Enable required extensions
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS "pg_stat_statements";

-- Create a read-only user for monitoring (optional)
-- CREATE USER spooled_readonly WITH PASSWORD 'readonly_password';
-- GRANT CONNECT ON DATABASE spooled TO spooled_readonly;
-- GRANT USAGE ON SCHEMA public TO spooled_readonly;
-- GRANT SELECT ON ALL TABLES IN SCHEMA public TO spooled_readonly;
-- ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO spooled_readonly;

-- Display initialization complete message
DO $$
BEGIN
    RAISE NOTICE 'Spooled database initialization complete';
END $$;
