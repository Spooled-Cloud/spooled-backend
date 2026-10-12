-- no-transaction
-- An index for the schedule_runs -> jobs foreign key.
--
-- `schedule_runs.job_id` references `jobs(id) ON DELETE SET NULL` and had no
-- index, so every deleted job made Postgres scan all of schedule_runs to clear
-- the references to it. The job retention sweeps delete thousands of jobs per
-- pass inside the scheduler loop: on a database seeded at production scale
-- (450k schedule_runs, 130k jobs, PG16) purging 2,000 dead-lettered jobs took
-- 55.1 s, 54.9 s of it in this foreign-key action (27 ms a job), and 5,000
-- expired completed jobs 140.9 s, while the loop that also fires cron
-- schedules and recovers leases waited. With this index: 185 ms and 341 ms.
--
-- Partial on `job_id IS NOT NULL`: the key action only ever looks up a job id,
-- and most rows lose their job id once retention removes the job, so the index
-- stays small.
--
-- CONCURRENTLY keeps schedule_runs writable while the index builds, which is
-- why this file runs outside a transaction. It is a single statement on
-- purpose: Postgres runs a multi-statement string as one implicit transaction
-- block and refuses CREATE INDEX CONCURRENTLY inside it, so this file cannot
-- carry the usual `SET lock_timeout` line. An interrupted build leaves an
-- INVALID index that IF NOT EXISTS skips on the next start; 20261013140001
-- rebuilds it then.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_schedule_runs_job_id
    ON schedule_runs (job_id)
    WHERE job_id IS NOT NULL;
