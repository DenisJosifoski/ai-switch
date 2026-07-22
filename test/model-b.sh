#!/bin/sh
# Model B — simulated llama-server that binds PORT and serves /v1/models.
# Used by integration tests to exercise switch_model under real conditions.

PORT="${PORT:?PORT env var is required}"

exec python3 -c "
import http.server, json, signal, sys, os

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/v1/models':
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            body = json.dumps({'object': 'list', 'data': [{'id': 'model-b'}]})
            self.wfile.write(body.encode())
        else:
            self.send_response(404)
            self.end_headers()
    def log_message(self, format, *args):
        pass  # suppress stderr noise

signal.signal(signal.SIGTERM, lambda s, f: os._exit(0))
signal.signal(signal.SIGINT, lambda s, f: os._exit(0))
try:
    server = http.server.HTTPServer(('127.0.0.1', int('${PORT}')), Handler)
except Exception as e:
    print(f'Failed to bind port {PORT}: {e}', file=sys.stderr)
    sys.exit(1)
server.serve_forever()
"
