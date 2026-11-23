-- Each ref keeps its own copy of a cache key, as on GitHub (ch-3xn). Before this a key was
-- unique per repository, so the first branch to save it kept the default branch from saving
-- its own, and a job may only read its own ref's, its base's and the default branch's.
ALTER TABLE caches DROP CONSTRAINT caches_repo_id_key_version_key;
ALTER TABLE caches ADD CONSTRAINT caches_repo_key_version_ref UNIQUE NULLS NOT DISTINCT (repo_id, key, version, git_ref);
