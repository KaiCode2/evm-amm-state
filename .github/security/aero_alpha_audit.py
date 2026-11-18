"""Exact Aero alpha acceptance; publication is checked separately from PR packaging."""
import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = 'evm-amm-state-0.3.1-alpha.10'
REPO = 'KaiCode2/evm-amm-state'
PR = 46
BASE = '5dca44f86f533065c0f4934ff29dbf2dcb74522e'
LOCK = '4c5f1a26029b85ea20a7c91f00363190878f515a56bb2be36605e6d3112542ae'
PAYLOAD = '2f12ab0465a75598add46ad17862ed53dc36b697dcadd2739f3b5eaeff48f9b8'
ARCHIVE = 'c9eeca201c264ffc6da831b59d0ec9d94438706292ab252b6ad8f10faa11d700'
WORKFLOW = 'e55313d466203cb38e5824c18bce4dd9afd30f4e05a4726c96f20ebace398452'
SOURCE = 'registry+https://github.com/rust-lang/crates.io-index'
BIGINT = ('RUSTSEC-2026-0387', 'crypto-bigint', '0.5.5', SOURCE,
          '0dc92fb57ca44df6db8059111ab3af99a63d5d0f8375d9972e319a379c6bab76')
# Kai approved the exact crate-only terms on 18 November 2026 at 15:39:41 UTC.
APPROVED = True
START = dt.datetime(2026, 11, 18, 15, 39, 41, tzinfo=dt.timezone.utc)
END = dt.datetime(2026, 11, 19, 16, tzinfo=dt.timezone.utc)
POLICY_FILES = {'.github/workflows/ci.yml', '.github/security/aero_alpha_audit.py',
                '.github/security/test_aero_alpha_audit.py', '.github/security/aero-alpha-approval.md',
                '.github/security/alpha10-preparation.md'}
QUALIFICATION_SOURCE = 'cfa549ba9773ce2d32dce2a17d2fb3244161b0df'
QUALIFICATION_TESTS = {
    'tests/reactive_ws_e2e.rs': '818cc8277a534431300be1b5f30409249abdb9f821f325e3a6133fa3769b9ac8',
    'tests/reactive_ws_e2e/lineage.rs': '18e18780a2055e4db0bb24dd998df79263d129ed39ac5780a3421362916a0fb5',
    'tests/reactive_ws_e2e/regression.rs': '9850120d4f5c786024dcc827e300b189904b39dec96aff62959acf628586435f',
    'tests/reactive_ws_e2e/strict.rs': 'a871b329b798ac03f7cd980ed84db231ab5cb70b5981967e1589283e457b5934',
}

# Complete current warning identities; these remain visible and new warnings refuse.
WARNINGS = {
    ('unmaintained', 'RUSTSEC-2024-0388', 'derivative', '2.2.0', 'registry+https://github.com/rust-lang/crates.io-index', 'fcc3dd5e9e9c0b295d6e1e4d811fb6f157d5ffd784b8d202fc62eac8035a770b'),
    ('unmaintained', 'RUSTSEC-2024-0436', 'paste', '1.0.15', 'registry+https://github.com/rust-lang/crates.io-index', '57c0d7b74b563b49d38dae00a0c37d4d6de9b432382b2892f0574ddcae73fd0a'),
    ('unmaintained', 'RUSTSEC-2025-0141', 'bincode', '1.3.3', 'registry+https://github.com/rust-lang/crates.io-index', 'b1f45e9417d87227c7a56d22e471c6206462cba514c7590c09aff4cf6d1ddcad'),
    ('unmaintained', 'RUSTSEC-2026-0173', 'proc-macro-error2', '2.0.1', 'registry+https://github.com/rust-lang/crates.io-index', '11ec05c52be0a07b08061f7dd003e7d7092e0472bc731b4af7bb1ef876109802'),
    ('unmaintained', 'RUSTSEC-2026-0441', 'pin-utils', '0.1.0', 'registry+https://github.com/rust-lang/crates.io-index', '8b870d8c151b6f2fb93e84a13146138f05d02ed11c7e7c54f8826aaaf7c9f184'),
    ('unmaintained', 'RUSTSEC-2026-0543', 'chrono', '0.4.45', 'registry+https://github.com/rust-lang/crates.io-index', '1aa79e62e7697b8e29b513a68abacf485adcd1fe8284a4316c5ae868e6633327'),
    ('unsound', 'RUSTSEC-2026-0253', 'lru', '0.16.4', 'registry+https://github.com/rust-lang/crates.io-index', '7f66e8d5d03f609abc3a39e6f08e4164ebf1447a732906d39eb9b99b7919ef39'),
    ('unsound', 'RUSTSEC-2026-0386', 'tokio', '1.52.3', 'registry+https://github.com/rust-lang/crates.io-index', '8fc7f01b389ac15039e4dc9531aa973a135d7a4135281b12d7c1bc79fd57fffe'),
    ('unsound', 'RUSTSEC-2026-0486', 'rustix', '1.1.4', 'registry+https://github.com/rust-lang/crates.io-index', 'b6fe4565b9518b83ef4f91bb47ce29620ca828bd32cb7e408f0062e9930ba190'),
}


def require(ok, reason):
    if not ok:
        raise ValueError(reason)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def verify_payload(archive, head, publication=False):
    files = {}
    provenance = None
    with tarfile.open(archive, 'r:gz') as tar:
        for member in tar:
            root, sep, name = member.name.partition('/')
            require(member.isfile() and sep and root == PACKAGE and name
                    and '..' not in Path(name).parts and name not in files,
                    'unexpected or duplicate package member')
            data = tar.extractfile(member).read()
            if name == '.cargo_vcs_info.json':
                require(provenance is None, 'duplicate VCS provenance')
                provenance = json.loads(data)
            else:
                files[name] = [member.mode, sha(data)]
    require(provenance == {'git': {'sha1': head}, 'path_in_vcs': ''}, 'unexpected package provenance')
    require(sha(json.dumps(files, sort_keys=True, separators=(',', ':')).encode()) == PAYLOAD,
            'packaged payload differs from approved alpha')
    if publication:
        require(head == BASE and sha(archive.read_bytes()) == ARCHIVE,
                'publication requires the exact approved source and archive bytes')


def findings(report, inactive, returncode):
    require(returncode == 1 and not report.get('error'), 'unexpected audit result')
    settings = report['settings']
    require(not any(settings[field] for field in ('ignore', 'target_arch', 'target_os', 'severity')),
            'audit report was filtered')
    require({'unmaintained', 'unsound', 'notice'} <= set(settings['informational_warnings']),
            'audit warning classes were filtered')
    summary = report['vulnerabilities']
    entries = summary['list']
    require(summary['count'] == len(entries) and summary['found'] == bool(entries), 'incomplete audit findings')
    rows = [(v['advisory']['id'], *(v['package'][key] for key in ('name', 'version', 'source', 'checksum')))
            for v in entries]
    require(len(rows) == 2 and set(rows) == {BIGINT, inactive}, 'unaccepted, absent or duplicate vulnerability')
    warning_rows = [(kind, entry['advisory']['id'],
                     *(entry['package'][key] for key in ('name', 'version', 'source', 'checksum')))
                    for kind, entries in report['warnings'].items() for entry in entries]
    require(len(warning_rows) == len(WARNINGS) and set(warning_rows) == WARNINGS,
            'unaccepted, absent or duplicate audit warning')
    require(re.fullmatch(r'[0-9a-f]{40}', report['database'].get('last-commit') or ''),
            'fresh advisory database identity unavailable')


def qualification_tests(root, head, changes):
    require(set(changes) - POLICY_FILES == set(QUALIFICATION_TESTS),
            'qualification requires the exact four excluded test changes')
    for parent, child in ((BASE, QUALIFICATION_SOURCE), (QUALIFICATION_SOURCE, head)):
        ancestry = subprocess.run(['git', 'merge-base', '--is-ancestor', parent, child],
                                  cwd=root, capture_output=True)
        require(ancestry.returncode == 0, 'qualification requires verified test-source ancestry')
    changed = subprocess.check_output(
        ['git', 'diff', '--name-only', '--no-renames', '-z', BASE, QUALIFICATION_SOURCE], cwd=root)
    require(set(filter(None, changed.decode().split('\0'))) == set(QUALIFICATION_TESTS),
            'qualified test source changes runtime or other files')
    for name, expected in QUALIFICATION_TESTS.items():
        source = subprocess.check_output(['git', 'show', QUALIFICATION_SOURCE + ':' + name], cwd=root)
        path = root / name
        require(sha(source) == expected and path.is_file() and not path.is_symlink()
                and path.resolve() == root.resolve() / name and sha(path.read_bytes()) == expected,
                'qualification test hash differs from reviewed source')


def qualify(root, now, head, changes, publication=False):
    require(APPROVED is True, 'exact alpha10 owner approval is absent')
    require(START <= now < END, 'alpha acceptance expired or not yet valid')
    require(not set(changes) - POLICY_FILES - set(QUALIFICATION_TESTS),
            'source changed outside qualification policy')
    require(sha((root / 'Cargo.lock').read_bytes()) == LOCK, 'lockfile differs from approved alpha')
    if publication:
        require(head == BASE and not changes, 'publication requires approved clean source')
    else:
        require(sha((root / '.github/workflows/ci.yml').read_bytes()) == WORKFLOW,
                'qualification workflow changed')
        qualification_tests(root, head, changes)


def github_scope(env, event):
    if env.get('GITHUB_EVENT_NAME') == 'push':
        require(env.get('GITHUB_REPOSITORY') == REPO
                and env.get('GITHUB_REF') == 'refs/heads/main'
                and event.get('ref') == 'refs/heads/main'
                and event.get('deleted') is False
                and event.get('repository', {}).get('full_name') == REPO,
                'not approved normal main promotion')
        return
    require(env.get('GITHUB_EVENT_NAME') == 'pull_request'
            and env.get('GITHUB_REPOSITORY') == REPO
            and env.get('GITHUB_REF') == f'refs/pull/{PR}/merge', 'not approved PR context')
    pr = event['pull_request']
    require(event['number'] == PR and pr['base']['ref'] == 'main'
            and pr['base']['repo']['full_name'] == REPO == pr['head']['repo']['full_name'],
            'unapproved PR, repository or promotion')


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--publication-check', action='store_true')
    parser.add_argument('--source-root', type=Path)
    parser.add_argument('--archive', type=Path)
    args = parser.parse_args()
    require(args.publication_check or args.source_root is None, 'source override only for exact publication check')
    require(not args.publication_check or args.source_root is not None, 'explicit publication source required')
    root = (args.source_root or ROOT).resolve()
    if os.environ.get('GITHUB_ACTIONS') == 'true':
        require(not args.publication_check, 'CI acceptance cannot publish')
        github_scope(os.environ, json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text()))
    version = subprocess.check_output(['cargo-audit', '--version'], text=True).strip()
    require(version == 'cargo-audit 0.22.2', 'unexpected cargo-audit version')
    subprocess.run(['git', 'merge-base', '--is-ancestor', BASE, 'HEAD'], cwd=root, check=True)
    subprocess.run(['git', 'diff', '--quiet', 'HEAD', '--'], cwd=root, check=True)
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    changes = list(filter(None, subprocess.check_output(
        ['git', 'diff', '--name-only', '--no-renames', '-z', BASE, 'HEAD'], cwd=root).decode().split('\0')))
    qualify(root, dt.datetime.now(dt.timezone.utc), head, changes, args.publication_check)
    archive = args.archive or root / 'target/package' / (PACKAGE + '.crate')
    verify_payload(archive, head, args.publication_check)
    subprocess.run(['bash', 'scripts/check-security-exceptions.sh'], cwd=root, check=True)
    # Separate cwd/database keeps project ignores and unrelated local entries out.
    with tempfile.TemporaryDirectory() as work:
        result = subprocess.run(['cargo-audit', 'audit', '--db', str(Path(work) / 'advisory-db'),
                                 '--json', '--file', str(root / 'Cargo.lock')],
                                cwd=work, capture_output=True, text=True)
    report = json.loads(result.stdout)
    output = root / 'target/aero-alpha-audit.json'
    output.parent.mkdir(exist_ok=True)
    output.write_text(result.stdout)
    if result.stderr:
        print(result.stderr, file=sys.stderr)
    if summary := os.environ.get('GITHUB_STEP_SUMMARY'):
        with Path(summary).open('a') as stream:
            stream.write('\n### Full Aero dependency audit\n\n```json\n' + result.stdout + '\n```\n')
    locked = tomllib.loads((root / 'Cargo.lock').read_text())['package']
    tracing = next(p for p in locked if p['name'] == 'tracing-subscriber' and p['version'] == '0.2.25')
    inactive = ('RUSTSEC-2025-0055', *(tracing[k] for k in ('name', 'version', 'source', 'checksum')))
    findings(report, inactive, result.returncode)
    qualify(root, dt.datetime.now(dt.timezone.utc), head, changes, args.publication_check)
    verify_payload(archive, head, args.publication_check)
    print(f'{PACKAGE}: exact scoped acceptance, expires {END.isoformat()}; archive {sha(archive.read_bytes())}.')
    print('Publication-check passed for approved bytes.' if args.publication_check else
          'PR qualification only. This policy-head archive is not approved for publication.')
    print('No worker publication, production signing, deployment or activation authority.')


if __name__ == '__main__':
    try:
        run()
    except (ValueError, KeyError, TypeError, OSError, StopIteration, tarfile.TarError,
            subprocess.CalledProcessError) as error:
        sys.exit(f'Aero alpha audit refused: {error}')
