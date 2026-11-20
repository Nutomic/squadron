-- Persist the full validator report (features/bugs/security) so the judge and
-- the lane detail view can be hydrated from the store.
ALTER TABLE lanes ADD COLUMN val_report_json TEXT;
