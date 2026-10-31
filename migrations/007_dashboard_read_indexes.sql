-- Without this persistent join index, every series refresh constructs an
-- automatic covering index of all retained environment rows in a temp file.
CREATE INDEX IF NOT EXISTS idx_environment_sample ON environment_samples(sample_id);
