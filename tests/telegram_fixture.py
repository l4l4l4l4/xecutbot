"""Loopback-only Telegram transport. All identities and messages are synthetic."""
import email.parser
import email.policy
import http.server
import json
import queue
import threading
import time

CHAT = -1002614784999
RESIDENT = {"id": 101, "is_bot": False, "first_name": "Resident", "username": "resident_test"}
FINANCIER = {"id": 202, "is_bot": False, "first_name": "Financier", "username": "rvbar_el3"}
BOT = {"id": 303, "is_bot": True, "first_name": "Xecut", "username": "xecutbot", "can_join_groups": True,
       "can_read_all_group_messages": True, "supports_inline_queries": False,
       "can_connect_to_business": False, "has_main_web_app": False}


def chat(chat_id=CHAT):
    return {"id": chat_id, "type": "supergroup", "title": "Synthetic smoke residents", "is_forum": True}


def command(update_id, message_id, text, author=RESIDENT, chat_id=CHAT):
    word = text.split()[0]
    return {"update_id": update_id, "message": {"message_id": message_id, "date": int(time.time()),
            "chat": chat(chat_id), "from": author, "message_thread_id": 14828, "is_topic_message": True,
            "text": text, "entities": [{"type": "bot_command", "offset": 0, "length": len(word)}]}}


def reaction(update_id, message_id, author=FINANCIER, old=False, new=True):
    approval = [{"type": "emoji", "emoji": "👍"}]
    return {"update_id": update_id, "message_reaction": {"chat": chat(), "message_id": message_id,
            "user": author, "date": int(time.time()), "old_reaction": approval if old else [],
            "new_reaction": approval if new else []}}


class Telegram:
    def __init__(self):
        self.updates = queue.Queue()
        self.calls = []
        self.condition = threading.Condition()
        self.message_id = 1000
        self.fail_receipt = False
        self.receipt_state = lambda: None
        self.receipts = []
        self.errors = []
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                method = self.path.rsplit('/', 1)[-1].lower()
                raw = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                kind = self.headers.get('Content-Type', '')
                if kind.startswith('multipart/form-data'):
                    message = email.parser.BytesParser(policy=email.policy.default).parsebytes(
                        b'Content-Type: ' + kind.encode() + b'\r\n\r\n' + raw)
                    payload = {}
                    for part in message.iter_parts():
                        name = part.get_param('name', header='content-disposition')
                        data = part.get_payload(decode=True)
                        payload[name] = data if part.get_filename() else data.decode()
                else:
                    payload = json.loads(raw or b'{}')
                if method == 'sendphoto' and isinstance(payload.get('photo'), str):
                    reference = payload['photo']
                    if reference.startswith('attach://'):
                        payload['photo'] = payload[reference.removeprefix('attach://')]
                result = True
                if method == 'getme':
                    result = BOT
                elif method == 'getwebhookinfo':
                    result = {"url": "", "has_custom_certificate": False, "pending_update_count": 0}
                elif method == 'getupdates':
                    try:
                        result = [owner.updates.get(timeout=0.2)]
                    except queue.Empty:
                        result = []
                elif method == 'getchatmember':
                    user_id = int(payload['user_id'])
                    user = {101: RESIDENT, 202: FINANCIER, 303: BOT}[user_id]
                    result = {"status": "member", "user": user}
                    if user_id == BOT['id']:
                        result.update(status="administrator", can_be_edited=False, is_anonymous=False,
                                      can_manage_chat=True, can_delete_messages=True, can_manage_video_chats=False,
                                      can_restrict_members=False, can_promote_members=False,
                                      can_change_info=False, can_invite_users=False)
                elif method == 'getchatmembercount':
                    result = 3
                elif method in ('sendmessage', 'sendphoto'):
                    owner.message_id += 1
                    result = {"message_id": owner.message_id, "date": int(time.time()),
                              "chat": chat(int(payload['chat_id'])), "from": BOT}
                    if method == 'sendmessage':
                        result['text'] = payload['text']
                    else:
                        result['photo'] = [{"file_id": "smoke-photo", "file_unique_id": "smoke-photo",
                                            "width": 1100, "height": 520}]
                        result['caption'] = payload.get('caption', '')
                elif method == 'editmessagetext':
                    result = {"message_id": int(payload['message_id']), "date": int(time.time()),
                              "chat": chat(int(payload['chat_id'])), "from": BOT, "text": payload['text']}
                elif method not in ('deletewebhook', 'setmycommands', 'setmessagereaction'):
                    owner.errors.append(f"Unexpected Telegram method: {method}")
                    self.send_error(500, owner.errors[-1])
                    return
                if method == 'setmessagereaction':
                    owner.receipts.append((payload, owner.receipt_state()))
                if method == 'setmessagereaction' and owner.fail_receipt:
                    owner.fail_receipt = False
                    response = {"ok": False, "error_code": 500, "description": "Injected receipt failure"}
                else:
                    response = {"ok": True, "result": result}
                body = json.dumps(response).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    # Cancellation closes the dispatcher's idle long poll. Never hide lost updates.
                    if method == 'getupdates' and result == []:
                        return
                    owner.errors.append(f'Client disconnected during {method}')
                    raise
                with owner.condition:
                    owner.calls.append((method, payload))
                    owner.condition.notify_all()

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.address = f'127.0.0.1:{self.server.server_port}'

    def wait_call(self, method, predicate=lambda p: True, after=0, timeout=15):
        end = time.monotonic() + timeout
        with self.condition:
            while True:
                for index, (kind, payload) in enumerate(self.calls[after:], after):
                    if kind == method and predicate(payload):
                        return index, payload
                remaining = end - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f'No {method} after {after}; fixture errors={self.errors}')
                self.condition.wait(remaining)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
