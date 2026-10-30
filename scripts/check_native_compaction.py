#!/usr/bin/env python3
"""Opt-in real app-server/fake-provider fixture. Run in a network-disabled container.
Only a separately supplied Codex binary is mounted; no credentials or live roots.
"""
import json, pathlib, subprocess, tempfile, threading, queue, time, http.server, os, sys
manual = False
# Exact native profiles this fixture has verified.
VERIFIED = ('0.164.0', '0.165.0', '0.166.0')
requests = []
gate = threading.Event()
compact = threading.Event()
disconnected = []

def event(kind, **kw):
    return {'type': kind, **kw}

def completed(name, tokens):
    return event('response.completed', response={'id': name, 'status': 'completed', 'output': [], 'usage': {'input_tokens': tokens, 'output_tokens': 0, 'total_tokens': tokens, 'input_tokens_details': {'cached_tokens': 0}}})

def message(name, text):
    return event('response.output_item.done', output_index=0, item={'id': name, 'type': 'message', 'role': 'assistant', 'status': 'completed', 'content': [{'type': 'output_text', 'text': text, 'annotations': []}]})

class Handler(http.server.BaseHTTPRequestHandler):

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        requests.append(json.loads(body))
        n = len(requests)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.end_headers()

        def send(e):
            self.wfile.write(('data: ' + json.dumps(e) + '\n\n').encode())
            self.wfile.flush()
        try:
            send(event('response.created', response={'id': f'r{n}'}))
            if n == 1:
                send(event('response.output_item.done', output_index=0, item={'id': 'f1', 'type': 'function_call', 'call_id': 'f1', 'name': 'nonexistent_fixture_tool', 'arguments': '{}'}))
                send(completed('r1', 500))
            elif n == 2 or manual:
                compact.set()
                assert gate.wait(20), 'gate timeout'
                send(message('summary', 'Synthetic compaction summary'))
                send(completed('r2', 10))
            else:
                send(message(f'm{n}', 'Synthetic continuation'))
                send(completed(f'r{n}', 10))
        except (BrokenPipeError, ConnectionResetError):
            disconnected.append(n)
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory() as tmp:
    home = pathlib.Path(tmp)
    port = server.server_address[1]
    (home / 'config.toml').write_text(f"""model = "gpt-5.4"\nmodel_provider = "fixture"\nmodel_auto_compact_token_limit = 200\napproval_policy = "never"\nsandbox_mode = "read-only"\n[analytics]\nenabled = false\n[features]\ninstant_interrupt = {('false' if '--no-instant-interrupt' in sys.argv else 'true')}\nenable_request_compression = false\n[model_providers.fixture]\nname = "Fixture"\nbase_url = "http://127.0.0.1:{port}/v1"\nwire_api = "responses"\nrequires_openai_auth = false\nsupports_websockets = false\n""")
    env = {'PATH': '/usr/bin:/bin', 'HOME': tmp, 'CODEX_HOME': tmp, 'TOKIO_WORKER_THREADS': '2'}
    err = open(home / 'stderr', 'w')
    p = subprocess.Popen([sys.argv[1], 'app-server'], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, text=True)
    q = queue.Queue()
    notes = []

    def reader():
        for line in p.stdout:
            try:
                q.put(json.loads(line))
            except ValueError:
                pass
    threading.Thread(target=reader, daemon=True).start()
    seq = 0

    def rpc(method, params, allow_error=False):
        global seq
        seq += 1
        p.stdin.write(json.dumps({'id': seq, 'method': method, 'params': params}) + '\n')
        p.stdin.flush()
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            x = q.get(timeout=20)
            if x.get('id') == seq:
                if 'error' in x:
                    if allow_error:
                        return x['error']
                    raise RuntimeError((method, x['error']['code']))
                return x['result']
            notes.append(x)
        raise RuntimeError('timeout ' + method)
    try:
        peer = rpc('initialize', {'clientInfo': {'name': 'fixture', 'version': '1'}, 'capabilities': {'experimentalApi': True}}).get('userAgent', '')
        native = next((v for v in VERIFIED if f'/{v} ' in peer), None)
        assert native, f'Fixture requires a native peer in {VERIFIED}'
        print('initialize', peer, flush=True)
        thread = rpc('thread/start', {'cwd': tmp, 'model': 'gpt-5.4', 'modelProvider': 'fixture'})['thread']['id']
        turn = rpc('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'inert first fixture', 'text_elements': []}]})['turn']['id']
        assert compact.wait(20), f'no compaction, requests {len(requests)}'
        ack = rpc('turn/steer', {'threadId': thread, 'expectedTurnId': turn, 'clientUserMessageId': 'fixture-steer', 'input': [{'type': 'text', 'text': 'inert-steer-marker', 'text_elements': []}]})
        time.sleep(0.3)
        assert len(requests) == 2 and (not disconnected), (len(requests), disconnected)
        gate.set()
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            x = q.get(timeout=20)
            notes.append(x)
            if x.get('method') == 'turn/completed':
                break
        assert any(('inert-steer-marker' in json.dumps(r.get('input')) for r in requests[2:])), 'missing steer continuation'
        assert 'inert-steer-marker' not in json.dumps(requests[1]), 'steer in compaction'
        automatic_evidence = {'same_turn_ack': ack['turnId'] == turn, 'requests': len(requests), 'compaction_connection_preserved': 2 not in disconnected, 'steer_after_compaction': True, 'interrupted_events': sum((x.get('method') == 'turn/completed' and x.get('params', {}).get('turn', {}).get('status') == 'interrupted' for x in notes))}
        assert automatic_evidence['interrupted_events'] == 0
        manual = True
        compact.clear()
        gate.clear()
        rpc('thread/compact/start', {'threadId': thread})
        assert compact.wait(20), 'dedicated compact request did not start'
        active = rpc('thread/turns/list', {'threadId': thread, 'itemsView': 'summary', 'sortDirection': 'desc', 'limit': 1})['data'][0]
        assert active['status'] == 'inProgress'
        rejected = rpc('turn/steer', {'threadId': thread, 'expectedTurnId': active['id'], 'clientUserMessageId': 'dedicated-steer', 'input': [{'type': 'text', 'text': 'must-not-inject', 'text_elements': []}]}, allow_error=True)
        assert rejected['code'] == -32600
        assert rejected['data']['codexErrorInfo']['activeTurnNotSteerable']['turnKind'] == 'compact'
        before = len(requests)
        time.sleep(0.3)
        assert len(requests) == before and before not in disconnected
        gate.set()
        print(json.dumps({'automatic': automatic_evidence, 'dedicated_compact_rejected': True, 'dedicated_compact_not_interrupted': True, 'native_version': native, 'provider': 'loopback_fixture', 'real_provider_calls': 0}))
    finally:
        gate.set()
        p.terminate()
        p.wait(timeout=10)
        server.shutdown()
