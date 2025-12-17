-- Fix workflows foreign key to include ON DELETE CASCADE
-- This allows organizations to be deleted without manual cleanup

-- Drop the existing constraint
ALTER TABLE workflows DROP CONSTRAINT IF EXISTS workflows_organization_id_fkey;

-- Re-add with CASCADE
ALTER TABLE workflows 
    ADD CONSTRAINT workflows_organization_id_fkey 
    FOREIGN KEY (organization_id) 
    REFERENCES organizations(id) 
    ON DELETE CASCADE;

-- Also fix job_dependencies which was missing CASCADE
ALTER TABLE job_dependencies DROP CONSTRAINT IF EXISTS job_dependencies_organization_id_fkey;

ALTER TABLE job_dependencies 
    ADD CONSTRAINT job_dependencies_organization_id_fkey 
    FOREIGN KEY (organization_id) 
    REFERENCES organizations(id) 
    ON DELETE CASCADE;

COMMENT ON TABLE workflows IS 'Stores workflow definitions and progress tracking. Cascades on org delete.';
COMMENT ON TABLE job_dependencies IS 'Stores job dependency relationships for workflow support. Cascades on org delete.';

