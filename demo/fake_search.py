#!/usr/bin/env python3
"""A tiny stand-in for a cluster's own search API, for demo recordings only.

Serves HTTPS on 127.0.0.2:443 with a wildcard certificate for the cluster hosts,
and answers the few calls the demo makes (/, _bulk, _count, _search) from memory.

Usage: fake_search.py CERT KEY
"""

import json
import ssl
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

CERT, KEY = sys.argv[1:3]
DOCS = {}  # index -> list of documents


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def cluster_id(self):
        return self.headers.get("Host", "").split(".")[0]

    def path_parts(self):
        url = urlsplit(self.path)
        return [p for p in url.path.split("/") if p], parse_qs(url.query)

    def do_GET(self):
        parts, query = self.path_parts()
        if not parts:
            return self.send(200, {
                "name": self.cluster_id(),
                "cluster_name": self.cluster_id(),
                "version": {"distribution": "opensearch", "number": "2.19.0"},
                "tagline": "The OpenSearch Project: https://opensearch.org/",
            })
        index, action = parts[0], parts[1] if len(parts) > 1 else ""
        docs = DOCS.get(index, [])
        if action == "_count":
            return self.send(200, {"count": len(docs), "_shards": {"total": 2, "successful": 2, "failed": 0}})
        if action == "_search":
            q = query.get("q", [""])[0].lower()
            hits = [d for d in docs if q in json.dumps(d).lower()] if q else docs
            return self.send(200, {
                "took": 3,
                "hits": {
                    "total": {"value": len(hits), "relation": "eq"},
                    "hits": [{"_index": index, "_id": str(i), "_source": d} for i, d in enumerate(hits[:3])],
                },
            })
        return self.send(404, {"error": "not found", "status": 404})

    def do_POST(self):
        parts, _ = self.path_parts()
        if not parts or parts[-1] != "_bulk":
            return self.send(404, {"error": "not found", "status": 404})
        default_index = parts[0] if len(parts) > 1 else None
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        lines = [l for l in raw.decode().splitlines() if l.strip()]
        items = []
        for action_line, source_line in zip(lines[0::2], lines[1::2]):
            meta = json.loads(action_line).get("index", {})
            index = meta.get("_index", default_index)
            DOCS.setdefault(index, []).append(json.loads(source_line))
            items.append({"index": {"_index": index, "result": "created", "status": 201}})
        return self.send(200, {"took": 38 + len(items) // 50, "errors": False, "items": items})


ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(CERT, KEY)
server = ThreadingHTTPServer(("127.0.0.2", 443), Handler)
server.socket = ctx.wrap_socket(server.socket, server_side=True)
server.serve_forever()
