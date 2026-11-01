#!/usr/bin/env python3
"""Een echte system-API-sync, harde QEMU-stop en herstel vanaf hetzelfde volume."""
import functools
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TARGET = 'aarch64-unknown-none-softfloat'
OUT = ROOT / 'target/qemu-sync'

class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass

def probe(binary=None, markers=('HOPOS_SYNC_WRITE','HOPOS_SYNC_READ'), output=OUT, job_name='syncprobe'):
    output.mkdir(parents=True, exist_ok=True)
    if binary is None:
        subprocess.run(['cargo', 'build', '--release', '--target', TARGET, '-p', 'syncprobe'], cwd=ROOT, check=True)
        binary = ROOT / 'target' / TARGET / 'release/syncprobe' 
    env = dict(os.environ, APP='hop')
    for key in ('ROLE', 'APPENV', 'ARTIFACT', 'WEBPORT', 'GUI'):
        env.pop(key, None)
    held = []
    for name in ('SYSPORT', 'AGENTPORT', 'LEADERPORT'):
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        env[name] = str(sock.getsockname()[1])
        held.append(sock)
    with tempfile.TemporaryDirectory(prefix='hopos-sync-') as tmp:
        tmp = Path(tmp)
        env['DISK'] = str(tmp / 'disk.img')
        # Stateful: de koude herstart moet het volume met de database terugvinden.
        env['BOOTARGS'] = 'hopos.storage=stateful'
        objcopy = subprocess.check_output(['rustc','--print','sysroot'],cwd=ROOT,text=True).strip()
        bins = list(Path(objcopy).glob('lib/rustlib/*/bin/rust-objcopy'))
        if not bins:
            raise RuntimeError('rust-objcopy ontbreekt')
        subprocess.run([str(bins[0]),'--strip-debug',str(binary),str(tmp/'syncprobe.elf')],check=True)
        server = http.server.ThreadingHTTPServer(('127.0.0.1',0), functools.partial(Quiet,directory=str(tmp)))
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever,daemon=True)
        thread.start()
        job = {'name':job_name,'driver':'hop','artifacts':[{'url':f'http://10.0.2.2:{server.server_port}/syncprobe.elf'}],
               'memory_limit':33554432,'volumes':{f'/volumes/{job_name}':'/data'}}
        for sock in held:
            sock.close()
        try:
            for boot, marker in enumerate(markers):
                log = output / f'boot-{boot}.log'
                with log.open('w') as stream:
                    p = subprocess.Popen(['sh',str(ROOT/'image/qemu-run.sh')],cwd=ROOT,env=env,
                        stdin=subprocess.DEVNULL,stdout=stream,stderr=subprocess.STDOUT,start_new_session=True)
                    posted = False
                    passed = False
                    try:
                        end = time.monotonic()+180
                        while time.monotonic()<end:
                            data = log.read_text(errors='replace')
                            if boot == 1 and markers[0] in data:
                                raise RuntimeError('koud hersteld volume mist de database')
                            if any(x in data for x in ('HOPOS_SYNC_FAIL','REPLICA_SQLITE_PERSIST_FAIL','HOPOS_PANIC','HOPOS_FATAL','HOPOS_EXCEPTION','HOPOS_APP_PANIC')):
                                raise RuntimeError(f'foutmarker: {log}')
                            if not posted and 'HOP_LEADER' in data and 'HOP_UP' in data:
                                req = urllib.request.Request(f"http://127.0.0.1:{env['LEADERPORT']}/v1/jobs",
                                    data=json.dumps(job).encode(),headers={'Content-Type':'application/json'},method='POST')
                                with urllib.request.urlopen(req,timeout=15) as response:
                                    if response.status not in (200,201,202):
                                        raise RuntimeError('jobspec geweigerd')
                                posted = True
                            if marker in data:
                                passed = True
                                print(f'PASS boot {boot}: {marker}',flush=True)
                                break
                            if p.poll() is not None:
                                break
                            time.sleep(0.05)
                        if not passed:
                            raise RuntimeError(f'marker ontbreekt: {marker}; zie {log}')
                    finally:
                        # Geen lifecycle-stop of afsluitende commit: echt abrupt weg.
                        if p.poll() is None:
                            os.killpg(p.pid,signal.SIGKILL)
                        p.wait(timeout=5)
                if boot == 0:
                    print('QEMU hard gestopt; hetzelfde volume start nu koud.',flush=True)
        finally:
            server.shutdown()
            server.server_close()
    (output/'checks.json').write_text(json.dumps({'sync_rpc':True,'cold_restore':True,'markers':markers},indent=2)+'\n')
    print(f'PASS: bevestigde sync en koude herstart; bewijs in {output}')

if __name__ == '__main__':
    probe()
