-- Instance ownership is a claim on the *instance*, not a set of account links.
--
-- Until now "the owner" was defined by `account_device_links`: the first
-- `/session/bootstrap` (or `/session/enroll`, once `[instance].owner_secret`
-- was set) adopted every account that belonged to nobody, so claiming the
-- instance literally wrote one link row per account. That conflated two
-- different questions — "who runs this instance?" and "which accounts may this
-- browser address?" — and it made the owner's reach a side effect of a bulk
-- write instead of a property of the instance.
--
-- This table holds the claim itself. It is a singleton: one row with `id = 1`,
-- or no row at all while the instance is unclaimed. `owner_token` is the same
-- opaque cookie value `account_device_links` stores. A claim carries no account
-- links, which is the point: clearing cookies costs nothing but re-proving the
-- secret, because `POST /session/enroll` re-points this row at the new token
-- and the account scope follows the claim, not the links.
CREATE TABLE instance_ownership (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    owner_token  TEXT NOT NULL,
    claimed_at   TEXT NOT NULL,
    claim_method TEXT NOT NULL CHECK (claim_method IN ('bootstrap', 'secret'))
) STRICT;
