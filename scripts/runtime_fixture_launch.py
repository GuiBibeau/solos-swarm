#!/usr/bin/env python3
"""Isolated synthetic-identity integration launcher; never a production issuer."""
import argparse
import json
import os
from pathlib import Path
import subprocess

p = argparse.ArgumentParser()
p.add_argument('root', type=Path)
p.add_argument('build', type=Path)
p.add_argument('binary', type=Path)
p.add_argument('--actual-local', action='store_true')
a = p.parse_args()
assert all(v.is_absolute() for v in (a.root, a.build, a.binary))
assert not a.root.exists(), 'use a fresh fixture directory'
node = '/opt/solos-swarm-lab/bin/node'
env = dict(os.environ, SOLOS_RUNTIME_SMOKE_ROOT=str(a.root), SOLOS_RUNTIME_SMOKE_BINARY=str(a.binary), SOLOS_RUNTIME_SMOKE_PI_ROOT='/opt/solos-swarm-lab/pi', SOLOS_RUNTIME_SMOKE_NODE=node, SOLOS_RUNTIME_SMOKE_CP_UID='999', SOLOS_RUNTIME_SMOKE_EVAL_UID='995', SOLOS_RUNTIME_SMOKE_EVAL_GID='984')
if a.actual_local:
    env.update(SOLOS_RUNTIME_SMOKE_ACTUAL_LOCAL='true', SOLOS_RUNTIME_SMOKE_TOKENIZER=str(a.build/'tokenizer.json'), SOLOS_RUNTIME_SMOKE_TEMPLATE=str(a.build/'gguf_chat_template.jinja'))
def run(args, **kw):
    subprocess.run([str(x) for x in args], check=True, **kw)
run([a.build/'fixture-preparer', '--ignored', 'prepare_runtime_fixture'], env=env)
runtime = json.loads((a.root/'cp/runtime.json').read_text())
profiles = list(runtime.get('evaluators', []))
if runtime.get('evaluator') is not None:
    profiles.append(runtime['evaluator'])
assert len(profiles) == 1 and profiles[0]['process']['uid'] == 995, 'unexpected synthetic evaluator profile'
delivery_gid = profiles[0]['process']['gid']
assert type(delivery_gid) is int and 0 < delivery_gid < 0xffffffff, 'invalid broker delivery group'
for directory, owner in [('cp','999:985'), ('shared','999:985'), ('witness','994:983'), ('evaluator','995:985')]:
    run(['sudo','chown','-R',owner,a.root/directory])
for directory in [a.root,a.root/'evaluator']:
    run(['sudo','chmod','711',directory])
for directory in ['evaluator/public','evaluator/ipc']:
    run(['sudo','chmod','710',a.root/directory])
for file in ['evaluator/public/evaluator.der','evaluator/public/ca.der']:
    run(['sudo','chmod','640',a.root/file])
name = a.root.name
script = a.build/'scripts/runtime_fixture.mjs'
def unit(suffix, uid, gid, memory, command, extra=()):
    run(['sudo','systemd-run',f'--unit={name}-{suffix}',f'--property=User={uid}',f'--property=Group={gid}',f'--property=MemoryMax={memory}',*extra,*command])
unit('issuer','debian','debian','1G',[node,script,'serve',a.root])
unit('witness',994,983,'1G',[a.binary,'mvp','witness-serve','--config',a.root/'witness/witness.json'])
unit('evaluator',995,985,'1G',[a.binary,'mvp','evaluator-serve','--bundle',a.root/'evaluator/protected/bundle.json','--certificate',a.root/'evaluator/public/evaluator.der','--private-key',a.root/'evaluator/protected/key.der','--ca',a.root/'evaluator/public/ca.der','--state',a.root/'evaluator/protected/state/private.json','--ipc-socket',a.root/'evaluator/ipc/service.sock','--broker-uid',999],[f'--property=SupplementaryGroups={delivery_gid}'])
unit('cp',999,985,'8G',[a.binary,'mvp','serve','--config',a.root/'cp/runtime.json'],['--property=TasksMax=4096','--setenv=XDG_RUNTIME_DIR=/run/user/999','--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/999/bus'])
run([node,script,'drive',a.root])
