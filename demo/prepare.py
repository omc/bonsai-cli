#!/usr/bin/env python3
"""Off-camera step for the demo recording, run inside the demo namespace.

  prepare.py CLUSTER_NAME   Ages the cluster a week in bonsai-mock, so
                            `bonsai cluster usage` has samples, then sets the
                            cluster's stats to the latest of them.

Reads DEMO_DB, DEMO_TOKEN and DEMO_ADMIN from the environment.
"""

import json
import os
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


cluster_id, created_at = cluster(sys.argv[1])
age(cluster_id, created_at)
s = latest_sample(cluster_id)
api(f"/_mock/clusters/{cluster_id}/stats", ADMIN,
    {"docs": s["docs"], "shards": s["shards"], "bytes": s["bytes"]}, method="POST")
