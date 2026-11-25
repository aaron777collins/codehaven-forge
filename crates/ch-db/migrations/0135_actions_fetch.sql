-- Actions from GitHub and from repositories here (ch-ctk.5): the site's policy, the archives
-- fetched, and which commit each run used.

-- One row. `selected` with a seed list by default; `none` allows only actions in the job's own
-- repository (`./path`), `all` every public GitHub action. Block always wins.
CREATE TABLE actions_policy (
    id                    BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    mode                  TEXT NOT NULL DEFAULT 'selected' CHECK (mode IN ('none', 'selected', 'all')),
    allow                 TEXT[] NOT NULL DEFAULT ARRAY[
        'actions/*', 'github/*', 'docker/*', 'dotnet/*', 'codecov/codecov-action', 'ruby/setup-ruby',
        'JamesIves/github-pages-deploy-action', 'peaceiris/actions-gh-pages', 'softprops/action-gh-release',
        'skills/action-update-step', 'dtolnay/rust-toolchain', 'Swatinem/rust-cache', 'peter-evans/create-pull-request'
    ],
    block                 TEXT[] NOT NULL DEFAULT '{}',
    require_sha           BOOLEAN NOT NULL DEFAULT false,
    allow_codehaven_repos BOOLEAN NOT NULL DEFAULT true,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO actions_policy (id) VALUES (true);

-- A tarball of one commit of an action's repository, in the blob store.
CREATE TABLE action_archives (
    host         TEXT NOT NULL CHECK (host IN ('github', 'haven')),
    name         TEXT NOT NULL,
    sha          TEXT NOT NULL,
    blob_sha256  TEXT NOT NULL,
    size         BIGINT NOT NULL,
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (host, name, sha)
);

-- What each `uses:` of a run resolved to. Later jobs and re-runs of failed jobs reuse it.
CREATE TABLE action_resolutions (
    run_id         BIGINT NOT NULL REFERENCES workflow_runs (id) ON DELETE CASCADE,
    host           TEXT NOT NULL CHECK (host IN ('github', 'haven')),
    name           TEXT NOT NULL,
    git_ref        TEXT NOT NULL,
    sha            TEXT NOT NULL,
    archive_sha256 TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, host, name, git_ref)
);
