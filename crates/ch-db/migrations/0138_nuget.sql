-- A private NuGet feed (ch-ctk.13): packages of kind 'nuget' beside the images, one row per
-- version. A version is written once. Its .nupkg lives in the blob store; the row holds a
-- reference on it, which the trigger gives back however the row goes (an unlisted version stays,
-- a deleted account or package takes its versions along).
ALTER TABLE packages DROP CONSTRAINT packages_kind_check;
ALTER TABLE packages ADD CONSTRAINT packages_kind_check CHECK (kind IN ('container', 'nuget'));

CREATE TABLE nuget_versions (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    package_id  BIGINT NOT NULL REFERENCES packages (id) ON DELETE CASCADE,
    -- Normalized: lower case, three or four numbers, no build metadata.
    version     TEXT NOT NULL,
    blob_sha256 TEXT NOT NULL,
    size        BIGINT NOT NULL,
    nuspec      TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    authors     TEXT NOT NULL DEFAULT '',
    listed      BOOLEAN NOT NULL DEFAULT true,
    pushed_by   BIGINT REFERENCES users (id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (package_id, version)
);

CREATE FUNCTION nuget_versions_release_blob() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE blobs SET refcount = GREATEST(refcount - 1, 0),
                     unreferenced_since = CASE WHEN refcount <= 1 THEN now() ELSE unreferenced_since END
     WHERE sha256 = OLD.blob_sha256;
    RETURN OLD;
END
$$;

CREATE TRIGGER nuget_versions_release BEFORE DELETE ON nuget_versions
    FOR EACH ROW EXECUTE FUNCTION nuget_versions_release_blob();
