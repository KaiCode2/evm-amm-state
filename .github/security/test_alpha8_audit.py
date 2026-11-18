import datetime as dt
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

import alpha8_audit as gate

INACTIVE = ("RUSTSEC-2025-0055", "tracing-subscriber", "0.2.25", gate.SOURCE, "fixture")


def report(*entries):
    rows = [{"advisory": {"id": r[0]}, "package": dict(zip(("name", "version", "source", "checksum"), r[1:]))}
            for r in entries]
    return {"settings": {"ignore": [], "target_arch": [], "target_os": [], "severity": None}, "vulnerabilities": {"list": rows, "count": len(rows), "found": bool(rows)}}


class DecisionTests(unittest.TestCase):
    def test_only_the_exact_registry_finding_can_be_accepted(self):
        self.assertTrue(gate.findings(report(INACTIVE, gate.BIGINT), INACTIVE))
        self.assertFalse(gate.findings(report(INACTIVE), INACTIVE))
        for index in range(5):
            other = list(gate.BIGINT)
            other[index] = "other"
            with self.subTest(index=index), self.assertRaises(ValueError):
                gate.findings(report(gate.BIGINT, tuple(other)), INACTIVE)
        with self.assertRaises(ValueError):
            gate.findings(report(gate.BIGINT, gate.BIGINT), INACTIVE)

    def test_hidden_findings_reject_the_report(self):
        value = report(gate.BIGINT, INACTIVE)
        value["settings"]["ignore"] = ["RUSTSEC-2026-0387"]
        with self.assertRaisesRegex(ValueError, "filtered"):
            gate.findings(value, INACTIVE)
        with self.assertRaisesRegex(ValueError, "inactive"):
            gate.findings(report(gate.BIGINT), INACTIVE)

    def test_approval_boundary_and_source_scope(self):
        for when, paths in [(gate.START - dt.timedelta(seconds=1), []), (gate.END, []),
                            (gate.START, ["src/lib.rs"]), (gate.START, ["Cargo.toml"])]:
            with self.subTest(when=when, paths=paths), self.assertRaises(ValueError):
                gate.qualify(None, when, gate.BASE, paths)

    def test_modified_or_dirty_archive_is_refused(self):
        for dirty in [True, False]:
            with tempfile.TemporaryDirectory() as directory:
                archive = Path(directory) / "test.crate"
                with tarfile.open(archive, "w:gz") as tar:
                    vcs = {"git": {"sha1": gate.BASE}, "path_in_vcs": ""}
                    if dirty:
                        vcs["git"]["dirty"] = True
                    for name, content in [(".cargo_vcs_info.json", json.dumps(vcs).encode()), ("src/lib.rs", b"changed")]:
                        item = tarfile.TarInfo(gate.PACKAGE + "/" + name)
                        item.size = len(content)
                        tar.addfile(item, io.BytesIO(content))
                with self.assertRaisesRegex(ValueError, "provenance" if dirty else "payload"):
                    gate.verify_payload(archive, gate.BASE)
