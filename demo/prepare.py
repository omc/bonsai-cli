#!/usr/bin/env python3
"""Off-camera steps for the demo recording, run inside the demo namespace.

  prepare.py dataset CLUSTER_NAME   Ages the cluster a week in bonsai-mock, so
                                    `bonsai usage` has samples, and writes
                                    products.ndjson with as many documents as the
                                    mock's latest sample holds.
  prepare.py stats CLUSTER_NAME     Sets the cluster's stats to that sample, so the
                                    numbers in `usage` match what was loaded.

Reads DEMO_DB, DEMO_TOKEN and DEMO_ADMIN from the environment.
"""

import json
import os
import random
import sqlite3
import sys
import urllib.request
from datetime import datetime, timedelta, timezone

MOCK = "http://127.0.0.1:4010"
DB, TOKEN, ADMIN = os.environ["DEMO_DB"], os.environ["DEMO_TOKEN"], os.environ["DEMO_ADMIN"]


def cluster(name):
    with sqlite3.connect(DB) as db:
        row = db.execute(
            "SELECT id, created_at FROM clusters WHERE name = ? AND terminated_at IS NULL", (name,)
        ).fetchone()
    return row


def api(path, token, body=None, method="GET"):
    req = urllib.request.Request(
        MOCK + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    raw = urllib.request.urlopen(req).read()
    return json.loads(raw) if raw else None


def latest_sample(cluster_id):
    now = datetime.now(timezone.utc).replace(microsecond=0)
    frm = (now - timedelta(hours=2)).strftime("%Y-%m-%dT%H:%M:%SZ")
    usage = api(f"/v1/clusters/{cluster_id}/usage?from={frm}", TOKEN)
    return usage["samples"][-1]


def age(cluster_id, created_at):
    # The same format the mock wrote (seconds, then any fraction and the zone),
    # a week earlier.
    head, rest = created_at[:19], created_at[19:]
    older = datetime.strptime(head, "%Y-%m-%dT%H:%M:%S") - timedelta(days=7)
    text = older.strftime("%Y-%m-%dT%H:%M:%S") + rest
    with sqlite3.connect(DB) as db:
        db.execute("UPDATE clusters SET created_at = ? WHERE id = ?", (text, cluster_id))


COLOURS = ["black", "navy", "olive", "rust", "grey", "cream", "forest", "denim", "charcoal", "sand"]
ITEMS = ["boots", "wool coat", "rain jacket", "beanie", "scarf", "sneakers", "parka", "gloves", "hoodie", "chinos"]


def dataset(name):
    cluster_id, created_at = cluster(name)
    age(cluster_id, created_at)
    n = latest_sample(cluster_id)["docs"]
    rng = random.Random(42)
    with open("products.ndjson", "w") as f:
        for i in range(n):
            colour, item = rng.choice(COLOURS), rng.choice(ITEMS)
            doc = {"sku": f"SKU-{i:06d}", "name": f"{colour.title()} {item}", "price": round(rng.uniform(12, 240), 2)}
            f.write('{"index":{}}\n' + json.dumps(doc) + "\n")


def stats(name):
    cluster_id, _ = cluster(name)
    s = latest_sample(cluster_id)
    api(f"/_mock/clusters/{cluster_id}/stats", ADMIN,
        {"docs": s["docs"], "shards": s["shards"], "bytes": s["bytes"]}, method="POST")


{"dataset": dataset, "stats": stats}[sys.argv[1]](sys.argv[2])
