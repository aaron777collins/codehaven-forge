-- Identity stream additions.

-- The last TOTP time step accepted for an account, so a code cannot be used twice.
ALTER TABLE users ADD COLUMN totp_last_step BIGINT;
