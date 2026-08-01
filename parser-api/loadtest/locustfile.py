"""SQLite concurrency load test for the E621 Account Parser backend.

Measures how the SQLite-backed API behaves under concurrent read/write
load. Three request classes simulate the real production mix:

  * SessionUser  — read-heavy: account reads + tag resolve
  * RecHeavyUser — the scoring hot path (recommendations) — heavy reads
  * WriteHeavy   — feedback interactions (feed_interactions inserts)

Tokens below are real `owner_token`s from `account_device_links`, so
each virtual user authenticates as a real linked account.

Run the server first:
    cd parser-api/loadtest && ROCKET_PORT=8088 \
        ../target/release/e621-account-parser-api

Then launch locust:
    locust -f loadtest/locustfile.py --host http://127.0.0.1:8088 \
        --headless -u 50 --spawn-rate 10 -t 2m
"""

import random

from locust import HttpUser, task, between

# Real owner_tokens from the production DB (each linked to account 658288 etc).
OWNER_TOKENS = [
    "hRbkZsk5BDlzAmsOs_W0Pcz8D6U6p2Q-7nRJUuqpWV8",
    "TRJsc9Kq4_KsThd6N05konRmz6uxAPF9uqYqKUVrTdo",
    "wXmETD2YIUXC0YTy_prtCF-Ld6hg8LdePQy2WOOZ7KI",
    "sqt0Z2RM_yvHYrtOa5mN9QQ-IcJQKSXD1rxGopyNI-Y",
    "uiMWLbEnqp9l9M2y0QCkPvZKtjH4GrUQS573SnbNOnY",
    "rEgzt6K7DscwHTn5q2TrL54RjRU7MlaUmchW-i9e_eQ",
    "l_9W9Y1WTxpgxG9pDlDMNSpjeToOiBR01CfBV58Bqac",
    "CTnVO70dweq7Rti4CsY_NQVm134Fp-FhYSoo3yP031E",
    "UF_jb3BDYxiIpn4etqSRgYyeFPtGi9r0XM0uUKBXVGs",
    "NhNMRIfuQHiO4OSywVpStIMUsYgoBGP_KxdeXLxAAfc",
    "Lc3pENttHDRqouWM1GAMx9p8QRF9hthQgBIl7ZsJ0Qw",
    "k8UPmTUk9TnchzvVDBx_D9LBs1M34hdbKLssoa9qDTg",
    "RAwl5wM5_X3DcfUhXSr4_XmprcRdPgBbjHCdJNrPCjo",
    "wi_Xhz65wU2Jb2APKmB9OKg2x19-ob0L2c5cEP75msM",
    "ni6Kd20rCMCjM9b46nFHm85-CSt8JuFFPozUYp3cSQ4",
    "NhU1UR9ZZ4MLwSlA7ifWlVO5dLymWAuPC9KETQS6gZI",
]
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
