"""SQLite concurrency load test for the E621 Account Parser backend.

Measure how the SQLite-backed API behaves under concurrent read/write
load. Three request classes simulate the real production mix:

  * SessionUser  — read-heavy: account reads + tag resolve
  * RecHeavyUser — the scoring hot path (recommendations) — heavy reads
  * WriteHeavy   — feedback interactions (feed_interactions inserts)

Tokens are read from the OWNER_TOKENS env var at runtime so no live
`owner_token` is ever committed to this repository.

Run the server first:
    cd parser-api/loadtest && ROCKET_PORT=8088 \
        ../target/release/e621-account-parser-api

Then launch locust:
    locust -f loadtest/locustfile.py --host http://127.0.0.1:8088 \
        --headless -u 50 --spawn-rate 10 -t 2m
"""

import os
import random

from locust import HttpUser, task, between

# WARNING: never commit live `owner_token` values — possession of a token is
# account ownership. Tokens are loaded at runtime from the OWNER_TOKENS env
# var (comma-separated) so this file stays secret-free and safe to commit.
#
#   OWNER_TOKENS="tok1,tok2" locust -f ...
#
# To make a token: POST /api/session/bootstrap to mint one, then link it to a
# real account via POST /api/account {id,name} (or reuse an existing linked
# token from your own deployment's `account_device_links`).
_DEFAULT_OWNER_TOKENS = [
    # Placeholders only — replace via OWNER_TOKENS env var at runtime.
    "REPLACE_ME_loadtest_token_1",
    "REPLACE_ME_loadtest_token_2",
]


def _owner_tokens() -> list[str]:
    raw = os.environ.get("OWNER_TOKENS", "").strip()
    if raw:
        return [t.strip() for t in raw.split(",") if t.strip()]
    return _DEFAULT_OWNER_TOKENS


OWNER_TOKENS = _owner_tokens()
TAGS = ["fluffy", "cat", "skeb", "artist", "outdoor", "scaly", "night", "indoor"]


class SessionUser(HttpUser):
    """Light read traffic: account reads + tag resolve."""
    wait_time = between(0.5, 2.0)

    def on_start(self):
        self.client.cookies.set("owner_token", random.choice(OWNER_TOKENS))

    @task(3)
    def tag_counts(self):
        self.client.get("/api/account/658288/tag_counts", name="/account/tag_counts")

    @task(2)
    def profile(self):
        self.client.get("/api/account/658288/profile", name="/account/profile")

    @task(2)
    def resolve_tag(self):
        tag = random.choice(TAGS)
        self.client.get(f"/api/tag/resolve?tag={tag}", name="/tag/resolve")

    @task(1)
    def list_accounts(self):
        self.client.get("/api/accounts", name="/accounts")


class RecHeavyUser(HttpUser):
    """Heavy scoring reads — the most expensive SQLite path."""
    wait_time = between(1.0, 3.0)

    def on_start(self):
        self.client.cookies.set("owner_token", random.choice(OWNER_TOKENS))

    @task(1)
    def recommendations(self):
        self.client.get("/api/recommendations/658288", name="/recommendations")


class WriteHeavy(HttpUser):
    """Write traffic: feed interactions insert into SQLite."""
    wait_time = between(0.2, 1.0)

    def on_start(self):
        self.client.cookies.set("owner_token", random.choice(OWNER_TOKENS))

    @task(1)
    def log_interaction(self):
        # Real post ids present in the production DB (FK constraint).
        post_id = random.choice([6439105, 6522523, 3910531, 6401001, 6550002, 6112003])
        event = random.choice(["qualified_impression", "open", "hide"])
        # POST requires a same-origin Origin header in release builds (CSRF).
        self.client.post(
            "/api/interaction",
            json={
                "account_id": 658288,
                "post_id": post_id,
                "event_type": event,
                "position": 3,
                "session_id": "loadtest",
            },
            headers={"Origin": "http://127.0.0.1:8088"},
            name="/interaction",
        )
