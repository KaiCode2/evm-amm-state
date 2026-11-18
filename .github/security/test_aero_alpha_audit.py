import copy
import datetime as dt
import io
import json
from pathlib import Path
from types import SimpleNamespace
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import aero_alpha_audit as gate

INACTIVE = ('RUSTSEC-2025-0055', 'tracing-subscriber', '0.2.25', gate.SOURCE, 'fixture')


def report(*entries):
    rows = [{'advisory': {'id': row[0]}, 'package': dict(zip(('name', 'version', 'source', 'checksum'), row[1:]))}
            for row in entries]
    warnings = {}
    for kind, advisory, name, version, source, checksum in gate.WARNINGS:
        warnings.setdefault(kind, []).append({'advisory': {'id': advisory},
            'package': dict(name=name, version=version, source=source, checksum=checksum)})
    return {'warnings': warnings, 'database': {'last-commit': 'a'*40},
            'settings': {'ignore': [], 'target_arch': [], 'target_os': [], 'severity': None,
                         'informational_warnings': ['unmaintained', 'unsound', 'notice']},
            'vulnerabilities': {'list': rows, 'count': len(rows), 'found': bool(rows)}}


class AlphaDecisionTests(unittest.TestCase):
    def test_exact_findings_only_and_complete_unfiltered_report(self):
        good = report(gate.BIGINT, INACTIVE)
        gate.findings(good, INACTIVE, 1)
        for index in range(5):
            other = list(gate.BIGINT);other[index] = 'other'
            with self.subTest(index=index), self.assertRaises(ValueError):
                gate.findings(report(INACTIVE, tuple(other)), INACTIVE, 1)
        for value in [report(INACTIVE), report(gate.BIGINT), report(gate.BIGINT, INACTIVE, INACTIVE)]:
            with self.assertRaises(ValueError):gate.findings(value, INACTIVE, 1)
        for key, value in [('ignore', ['RUSTSEC-2026-0387']), ('target_arch', ['x86_64']),
                           ('target_os', ['linux']), ('severity', 'critical'), ('informational_warnings', [])]:
            changed=copy.deepcopy(good);changed['settings'][key]=value
            with self.subTest(key=key), self.assertRaises(ValueError):gate.findings(changed, INACTIVE, 1)
        for code in [0,2,101]:
            with self.assertRaises(ValueError):gate.findings(good, INACTIVE, code)
        for value in [None, '', 'not-a-commit']:
            changed=copy.deepcopy(good);changed['database']['last-commit']=value
            with self.assertRaises(ValueError):gate.findings(changed, INACTIVE, 1)
        changed=copy.deepcopy(good);changed['vulnerabilities']['count']=1
        with self.assertRaises(ValueError):gate.findings(changed, INACTIVE, 1)

    def test_time_source_lock_and_workflow_are_bound(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);(root/'.github/workflows').mkdir(parents=True)
            (root/'Cargo.lock').write_text('lock');(root/'.github/workflows/ci.yml').write_text('workflow')
            with patch.object(gate, 'APPROVED', True),patch.object(gate, 'LOCK',gate.sha(b'lock')),patch.object(gate,'WORKFLOW',gate.sha(b'workflow')),patch.object(gate,'qualification_tests'):
                gate.qualify(root,gate.START,gate.BASE,[])
                gate.qualify(root,gate.START,gate.BASE,[],True)
                for now,head,changes,pub in [
                    (gate.START-dt.timedelta(seconds=1),gate.BASE,[],False),(gate.END,gate.BASE,[],False),
                    (gate.START,gate.BASE,['src/lib.rs'],False),(gate.START,'other',[],True),
                    (gate.START,gate.BASE,['.github/workflows/ci.yml'],True)]:
                    with self.assertRaises(ValueError):gate.qualify(root,now,head,changes,pub)
                (root/'Cargo.lock').write_text('changed')
                with self.assertRaises(ValueError):gate.qualify(root,gate.START,gate.BASE,[])
                (root/'Cargo.lock').write_text('lock');(root/'.github/workflows/ci.yml').write_text('changed')
                with self.assertRaises(ValueError):gate.qualify(root,gate.START,gate.BASE,[])

    def test_only_reviewed_excluded_tests_qualify_and_publication_stays_original(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp); (root/'.github/workflows').mkdir(parents=True)
            (root/'Cargo.lock').write_bytes(b'lock')
            (root/'.github/workflows/ci.yml').write_bytes(b'workflow')
            contents = {name: name.encode() for name in gate.QUALIFICATION_TESTS}
            hashes = {name: gate.sha(data) for name, data in contents.items()}
            for name, data in contents.items():
                path = root/name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(data)
            def git_output(command, **kwargs):
                self.assertEqual(kwargs['cwd'], root)
                if command[1] == 'diff':
                    self.assertEqual(command[2:], ['--name-only', '--no-renames', '-z', gate.BASE, gate.QUALIFICATION_SOURCE])
                    return ('\0'.join(contents)+'\0').encode()
                self.assertEqual(command[:2], ['git', 'show'])
                return contents[command[2].removeprefix(gate.QUALIFICATION_SOURCE+':')]
            changes = sorted(contents)
            with patch.object(gate, 'APPROVED', True), patch.object(gate, 'LOCK', gate.sha(b'lock')), \
                 patch.object(gate, 'WORKFLOW', gate.sha(b'workflow')), patch.object(gate, 'QUALIFICATION_TESTS', hashes), \
                 patch.object(gate.subprocess, 'check_output', side_effect=git_output), \
                 patch.object(gate.subprocess, 'run', return_value=SimpleNamespace(returncode=0)) as ancestry:
                gate.qualify(root, gate.START, gate.QUALIFICATION_SOURCE, changes)
                self.assertEqual([call.args[0][-2:] for call in ancestry.call_args_list],
                                 [[gate.BASE, gate.QUALIFICATION_SOURCE], [gate.QUALIFICATION_SOURCE, gate.QUALIFICATION_SOURCE]])
                for changed in [changes+['tests/extra.rs'], changes+['src/lib.rs'], changes+['Cargo.toml'], changes[:-1]]:
                    with self.subTest(changes=changed), self.assertRaises(ValueError):
                        gate.qualify(root, gate.START, gate.QUALIFICATION_SOURCE, changed)
                first = root/changes[0]; first.write_bytes(b'wrong test bytes')
                with self.assertRaisesRegex(ValueError, 'test hash'):
                    gate.qualify(root, gate.START, gate.QUALIFICATION_SOURCE, changes)
                first.write_bytes(contents[changes[0]])
                with patch.object(gate.subprocess, 'run', return_value=SimpleNamespace(returncode=1)), \
                     self.assertRaisesRegex(ValueError, 'ancestry'):
                    gate.qualify(root, gate.START, gate.BASE, changes)
                with patch.object(gate.subprocess, 'check_output', return_value=b'src/lib.rs\0'), \
                     self.assertRaisesRegex(ValueError, 'runtime or other'):
                    gate.qualify(root, gate.START, gate.QUALIFICATION_SOURCE, changes)
                with patch.object(gate, 'qualification_tests') as tests:
                    gate.qualify(root, gate.START, gate.BASE, [], True)
                    for head, changed in [(gate.QUALIFICATION_SOURCE, changes), (gate.QUALIFICATION_SOURCE, []), (gate.BASE, changes)]:
                        with self.assertRaisesRegex(ValueError, 'approved clean source'):
                            gate.qualify(root, gate.START, head, changed, True)
                    tests.assert_not_called()

    def test_fork_promotion_manual_and_other_pr_context_are_rejected(self):
        env={'GITHUB_EVENT_NAME':'pull_request','GITHUB_REPOSITORY':gate.REPO,'GITHUB_REF':f'refs/pull/{gate.PR}/merge'}
        event={'number':gate.PR,'pull_request':{'base':{'ref':'main','repo':{'full_name':gate.REPO}},'head':{'repo':{'full_name':gate.REPO}}}}
        gate.github_scope(env,event)
        for key,value in [('GITHUB_EVENT_NAME','push'),('GITHUB_EVENT_NAME','workflow_dispatch'),
                          ('GITHUB_REF','refs/heads/release'),('GITHUB_REPOSITORY','other')]:
            with self.assertRaises(ValueError):gate.github_scope({**env,key:value},event)
        for side in ['head','base']:
            changed=copy.deepcopy(event);changed['pull_request'][side]['repo']['full_name']='other'
            with self.assertRaises(ValueError):gate.github_scope(env,changed)
        changed=copy.deepcopy(event);changed['pull_request']['base']['ref']='release'
        with self.assertRaises(ValueError):gate.github_scope(env,changed)
        with self.assertRaises(ValueError):gate.github_scope(env,{**event,'number':99999})


    def test_preparation_has_no_approval_and_warning_drift_refuses(self):
        with tempfile.TemporaryDirectory() as temp:
            with patch.object(gate, 'APPROVED', False), self.assertRaisesRegex(ValueError, 'approval is absent'):
                gate.qualify(Path(temp), gate.START, gate.BASE, [])
        for mutation in ('missing', 'duplicate', 'new', 'filtered'):
            value = report(gate.BIGINT, INACTIVE)
            entries = value['warnings']['unmaintained']
            if mutation == 'missing': entries.pop()
            elif mutation == 'duplicate': entries.append(copy.deepcopy(entries[0]))
            elif mutation == 'new': entries[0]['advisory']['id'] = 'NEW'
            else: value.pop('warnings')
            with self.subTest(mutation=mutation), self.assertRaises((ValueError, KeyError)):
                gate.findings(value, INACTIVE, 1)

    def test_only_exact_repository_main_push_is_normal_promotion(self):
        env = {'GITHUB_EVENT_NAME': 'push', 'GITHUB_REPOSITORY': gate.REPO,
               'GITHUB_REF': 'refs/heads/main'}
        event = {'ref': 'refs/heads/main', 'deleted': False,
                 'repository': {'full_name': gate.REPO}}
        gate.github_scope(env, event)
        for key, value in [('GITHUB_REF', 'refs/heads/release'), ('GITHUB_REPOSITORY', 'other'),
                           ('GITHUB_EVENT_NAME', 'workflow_dispatch')]:
            with self.assertRaises((ValueError, KeyError)):
                gate.github_scope({**env, key: value}, event)
        with self.assertRaises(ValueError): gate.github_scope(env, {**event, 'deleted': True})

    def test_publication_cannot_use_policy_head_archive_or_changed_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            archive=Path(temp)/'test.crate'
            def write(head,dirty=False,extra=False):
                vcs={'git':{'sha1':head},'path_in_vcs':''}
                if dirty:vcs['git']['dirty']=True
                with tarfile.open(archive,'w:gz') as tar:
                    for name,data in [('src/lib.rs',b'approved'),('.cargo_vcs_info.json',json.dumps(vcs).encode())]+([('extra',b'x')] if extra else []):
                        m=tarfile.TarInfo(gate.PACKAGE+'/'+name);m.size=len(data);m.mode=0o644;tar.addfile(m,io.BytesIO(data))
            write(gate.BASE)
            payload=gate.sha(json.dumps({'src/lib.rs':[0o644,gate.sha(b'approved')]},sort_keys=True,separators=(',',':')).encode())
            with patch.object(gate,'PAYLOAD',payload),patch.object(gate,'ARCHIVE',gate.sha(archive.read_bytes())):
                gate.verify_payload(archive,gate.BASE,True)
                write('b'*40)
                gate.verify_payload(archive,'b'*40)
                with self.assertRaises(ValueError):gate.verify_payload(archive,'b'*40,True)
                write(gate.BASE,dirty=True)
                with self.assertRaises(ValueError):gate.verify_payload(archive,gate.BASE)
                write(gate.BASE,extra=True)
                with self.assertRaises(ValueError):gate.verify_payload(archive,gate.BASE)


if __name__ == '__main__':unittest.main()
