"""Run the real Rust dispatcher against loopback Telegram and a disposable ledger."""
import argparse
from contextlib import closing
from datetime import datetime
import html
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import struct
import subprocess
import tempfile
import time
from zoneinfo import ZoneInfo
import zlib

from telegram_fixture import CHAT, FINANCIER, RESIDENT, Telegram, chat, command, reaction


class Smoke:
    def __init__(self, binary, directory, output):
        self.binary = binary.resolve()
        self.directory = directory
        self.output = output.resolve()
        self.db = directory / 'ledger.sqlite'
        self.config = directory / 'config.json'
        self.telegram = Telegram()
        self.telegram.receipt_state = self.snapshot
        self.process = None
        self.log = None
        self.sequence = 0
        self.source = 0
        self.checks = []
        self.actions = []
        self.month = datetime.now(ZoneInfo('Europe/Belgrade')).strftime('%Y-%m')
        year, month = map(int, self.month.split('-'))
        self.config.write_text(json.dumps({
            'telegram_bot': {'bot_token': '303:verification', 'private_chat_id': CHAT,
                             'public_chat_id': -100777888999, 'public_channel_id': -100777888999,
                             'alert_chat_id': CHAT, 'money': {
                                 'enabled': True, 'minimum_monthly_euros': '50.00',
                                 'financier_user_id': FINANCIER['id'], 'contact': '@rvbar_el3',
                                 'start_month': f'{year - 1:04}-{month:02}',
                                 'initial_member_ids': [RESIDENT['id'], FINANCIER['id']],
                                 'initial_roster_complete': True, 'chart_months': 12}},
            'db': {'sqlite_path': f'sqlite://{self.db}?mode=rwc'},
            'backend': {'enable_update': False}}), encoding='utf-8')

    def query(self, sql, args=()):
        with closing(sqlite3.connect(self.db.as_uri() + '?mode=ro', uri=True, timeout=5)) as connection:
            return connection.execute(sql, args).fetchall()

    def snapshot(self):
        return {'balance': self.query('SELECT COALESCE(SUM(amount),0) FROM money_ledger')[0][0],
                'ledger': self.query('SELECT COUNT(*) FROM money_ledger')[0][0],
                'confirmed': self.query('SELECT COUNT(*) FROM money_claims WHERE confirmed=1')[0][0]}

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print(f'PASS {description}', flush=True)

    def start(self):
        self.log = (self.directory / f'bot-{len(self.checks)}.log').open('w')
        env = {k: v for k, v in os.environ.items()
               if not k.upper().startswith('XECUT_MONEY_SMOKE')
               and k.upper() not in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY')}
        env.update(NO_PROXY='127.0.0.1,localhost,::1', RUST_LOG='warn')
        self.process = subprocess.Popen([str(self.binary), str(self.config), self.telegram.address],
                                        stdout=self.log, stderr=self.log, env=env)

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGINT)
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
                    raise AssertionError('Dispatcher did not stop after cancellation')
            code = self.process.returncode
            self.process = None
            self.log.close()
            self.log = None
            if code != 0:
                raise AssertionError(f'Dispatcher exited with code {code}')

    def send(self, text, author=RESIDENT, room=CHAT, source=None, anonymous=False):
        self.sequence += 1
        self.source += 1
        source = self.source if source is None else source
        update = command(self.sequence, source, text, author=author, chat_id=room)
        if anonymous:
            update['message']['sender_chat'] = chat()
        self.actions.append((author['id'], text))
        after = len(self.telegram.calls)
        self.telegram.updates.put(update)
        _, payload = self.telegram.wait_call('sendmessage', lambda p: (
            int(p['chat_id']) == room and
            int(p.get('reply_parameters', {}).get('message_id', -1)) == source), after=after)
        return source, payload['text']

    def approve(self, card, author=FINANCIER, old=False, new=True):
        self.sequence += 1
        self.telegram.updates.put(reaction(self.sequence, card, author, old=old, new=new))
        self.actions.append((author['id'], f'reaction {card}: {"👍" if new else "removed"}'))

    def table(self):
        return self.send('/money@xecutbot paid 3')[1]

    def resident_row(self, table, name):
        return next(line for line in table.splitlines() if line.startswith(name))

    def claim(self, amount):
        source, _ = self.send(f'/money@xecutbot\tpay {amount} {self.month}')
        self.table()  # Same-chat dispatcher barrier after card attachment.
        row = self.query('SELECT id,card_id FROM money_claims WHERE source_id=?', (source,))[0]
        self.check(row[1] is not None, 'Payment card bound to a durable claim')
        return source, row[0], row[1]

    def receipts(self, card, after):
        self.telegram.wait_call('setmessagereaction', lambda p: int(p['message_id']) == card,
                                after=after)
        self.table()
        observed = next(state for payload, state in reversed(self.telegram.receipts)
                        if int(payload['message_id']) == card)
        self.check(observed['confirmed'] == observed['ledger'] and observed['balance'] > 0,
                   'Ledger was already committed when Telegram received bot acknowledgement')

    def safety(self):
        config = json.loads(self.config.read_text())
        unexpected = self.directory / 'unexpected.sqlite'
        config['db']['sqlite_path'] = f'sqlite://{unexpected}?mode=rwc'
        path = self.directory / 'wrong-db.json'
        path.write_text(json.dumps(config), encoding='utf-8')
        process = subprocess.Popen([str(self.binary), str(path), self.telegram.address],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            process.communicate(timeout=1)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=5)
        self.check(process.returncode != 0 and not unexpected.exists(),
                   'Smoke launcher refuses an alternative ledger before opening SQLite')

    def run(self):
        self.safety()
        self.start()
        _, help_text = self.send('/help@xecutbot money')
        self.check('/money@xecutbot pay' in help_text and '/money@xecutbot paid' in help_text,
                   'Financial help exposes suffixed parameterized commands')
        self.check(self.snapshot() == {'balance': 0, 'ledger': 0, 'confirmed': 0},
                   'Wallet starts at zero without ledger entries')
        self.check('❌' in self.resident_row(self.table(), 'Financier'),
                   'Imported silent resident appears as owing contributions')

        source, claim, card = self.claim('50')
        self.check(self.snapshot()['balance'] == 0, 'Unconfirmed claim is not cash')
        self.check('🕓' in self.resident_row(self.table(), 'Resident'),
                   'Pending claim is visible but not marked paid')
        self.approve(card, author={**RESIDENT, 'username': 'rvbar_el3'})
        self.table()
        self.check(self.snapshot()['ledger'] == 0,
                   'Matching financier username without numeric authority cannot approve')
        after = len(self.telegram.calls)
        self.approve(card)
        self.receipts(card, after)
        self.check(self.snapshot() == {'balance': 5000, 'ledger': 1, 'confirmed': 1},
                   'Financier approval posts exactly EUR 50')
        self.check('✅' in self.resident_row(self.table(), 'Resident'),
                   'Confirmed minimum contribution is marked paid')
        self.approve(card, old=True, new=False)
        self.approve(card)
        self.send(f'/money@xecutbot pay 999 {self.month}', source=source)
        self.table()
        self.check(self.snapshot()['balance'] == 5000 and self.snapshot()['ledger'] == 1
                   and self.query('SELECT amount,card_id FROM money_claims WHERE id=?', (claim,))[0]
                   == (5000, card), 'Reaction replay and conflicting command replay cannot alter cash or claim')

        _, _, second_card = self.claim('25')
        after = len(self.telegram.calls)
        self.telegram.fail_receipt = True
        self.approve(second_card)
        self.receipts(second_card, after)
        self.check(self.snapshot()['balance'] == 7500 and self.snapshot()['ledger'] == 2
                   and self.query('SELECT acknowledged FROM money_claims WHERE card_id=?',
                                  (second_card,))[0][0] == 0,
                   'Failed bot reaction leaves committed money and pending acknowledgement')
        self.stop()
        after = len(self.telegram.calls)
        self.start()
        self.receipts(second_card, after)
        self.check(self.snapshot()['balance'] == 7500 and self.snapshot()['ledger'] == 2
                   and self.query('SELECT acknowledged FROM money_claims WHERE card_id=?',
                                  (second_card,))[0][0] == 1,
                   'Restart retries acknowledgement without reposting money')

        for author, room, anonymous in ((RESIDENT, CHAT, False),
                                        (FINANCIER, -100987650001, False),
                                        (FINANCIER, CHAT, True)):
            self.send('/money@xecutbot expense 20 rejected-attempt', author=author,
                      room=room, anonymous=anonymous)
        self.table()
        self.check(self.snapshot()['balance'] == 7500 and self.snapshot()['ledger'] == 2,
                   'Unauthorized, foreign-chat and anonymous expenses do not mutate the wallet')

        source, _ = self.send('/money@xecutbot expense 30 supplies', author=FINANCIER)
        self.table()
        expense = self.query('SELECT id FROM money_ledger WHERE source_id=?', (source,))[0][0]
        self.check(self.snapshot()['balance'] == 4500, 'Expense reduces the wallet by EUR 30')
        self.send('/money@xecutbot income 5 donation', author=FINANCIER)
        self.send(f'/money@xecutbot reverse {expense} expense-correction', author=FINANCIER)
        payment = self.query('SELECT id FROM money_ledger WHERE claim_id=?', (claim,))[0][0]
        self.send(f'/money@xecutbot reverse {payment} payment-correction', author=FINANCIER)
        self.table()
        self.check(self.snapshot() == {'balance': 3000, 'ledger': 6, 'confirmed': 2}
                   and self.query('SELECT amount FROM money_ledger WHERE id=?', (payment,))[0][0] == 5000,
                   'Income and reversals produce EUR 30 while preserving original audit entries')
        self.check('🟡' in self.resident_row(self.table(), 'Resident'),
                   'Reversed payment leaves the remaining EUR 25 as a partial contribution')
        self.send(f'/money@xecutbot reverse {payment} duplicate-correction', author=FINANCIER)
        self.send('/money@xecutbot history 20', author=FINANCIER)
        self.check(self.snapshot()['balance'] == 3000 and self.snapshot()['ledger'] == 6,
                   'Repeated reversal cannot refund twice')

        after = len(self.telegram.calls)
        source, text = self.send('/money@xecutbot balance 12')
        amounts = re.findall(r'(-?\d+\.\d{2}) EUR', html.unescape(text))
        self.check(amounts[0] == '30.00', 'Displayed wallet agrees with the durable ledger')
        _, photo = self.telegram.wait_call('sendphoto', lambda p: int(
            json.loads(p['reply_parameters'])['message_id']) == source, after=after)
        png = photo['photo']
        self.check(png.startswith(b'\x89PNG\r\n\x1a\n'), 'Annual chart is an actual PNG attachment')
        width, height, depth, color, _, _, interlace = struct.unpack('>IIBBBBB', png[16:29])
        chunks, position = [], 8
        while position < len(png):
            size = struct.unpack('>I', png[position:position + 4])[0]
            kind = png[position + 4:position + 8]
            data = png[position + 8:position + 8 + size]
            expected = struct.unpack('>I', png[position + 8 + size:position + 12 + size])[0]
            self.check(zlib.crc32(kind + data) == expected, f'PNG {kind.decode()} checksum is valid')
            if kind == b'IDAT':
                chunks.append(data)
            position += size + 12
        self.check(depth == 8 and color in (2, 6) and interlace == 0
                   and len(zlib.decompress(b''.join(chunks))) == height * (1 + width * (3 if color == 2 else 4)),
                   'Annual chart contains complete decodable pixel data')
        self.stop()
        self.check(not self.telegram.errors, 'No unsupported Telegram calls occurred')
        self.output.mkdir(parents=True, exist_ok=True)
        (self.output / 'balance.png').write_bytes(png)
        (self.output / 'report.json').write_text(json.dumps({
            'synthetic_only': True, 'checks': self.checks, 'final': self.snapshot()},
            ensure_ascii=False, indent=2), encoding='utf-8')
        sections = ['<!doctype html><meta charset="utf-8"><title>Synthetic money smoke</title>',
                    '<style>body{max-width:72rem;margin:1.5rem auto;padding:0 1rem;font:16px/1.5 system-ui}'
                    'article,pre{white-space:pre-wrap;overflow-wrap:anywhere}img{max-width:100%;height:auto}</style>',
                    '<h1>Synthetic money smoke — not a Telegram client</h1>',
                    '<p>Fake identities, loopback API, temporary SQLite. No real money or Telegram sends.</p>',
                    '<h2>Resident/financier actions</h2><pre>' + html.escape('\n'.join(
                        f'{actor}: {text}' for actor, text in self.actions)) + '</pre>', '<h2>Actual bot responses</h2>']
        for method, payload in self.telegram.calls:
            if method in ('sendmessage', 'editmessagetext'):
                text = payload['text']
                if payload.get('parse_mode') != 'HTML':
                    text = html.escape(text)
                sections.append('<article>' + text + '</article><hr>')
        sections.append('<h2>Actual annual balance chart</h2><img src="balance.png" alt="Annual EUR balance">')
        (self.output / 'transcript.html').write_text('\n'.join(sections), encoding='utf-8')
        print(f'Artifacts: {self.output}/transcript.html and balance.png', flush=True)

    def close(self):
        try:
            self.stop()
        finally:
            self.telegram.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bot', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=Path('target/smoke-money'))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='xecutbot-money-smoke-') as directory:
        smoke = Smoke(args.bot, Path(directory), args.output)
        try:
            smoke.run()
        except BaseException:
            for path in Path(directory).glob('bot-*.log'):
                print(f'--- {path.name} ---\n{path.read_text()}', flush=True)
            raise
        finally:
            smoke.close()


if __name__ == '__main__':
    main()
