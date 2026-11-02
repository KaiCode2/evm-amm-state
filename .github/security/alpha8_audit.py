"""Qualification-only decision for the approved Aero alpha package."""
import datetime as dt
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = "evm-amm-state-0.3.1-alpha.8"
BASE = "829df3b10c38d04f320451b1d1cabb0fc5f0aba8"
LOCK = "057c6b5410aff6997abd858f1d7e53a388f3c1953f8b93061fe805510ef82e12"
PAYLOAD = "ed84560500bfe73dcc27df5dd7e27cbc8f0b3265e3d99b23242dc857252bd499"
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
BIGINT = ("RUSTSEC-2026-0387", "crypto-bigint", "0.5.5", SOURCE,
          "0dc92fb57ca44df6db8059111ab3af99a63d5d0f8375d9972e319a379c6bab76")
START = dt.datetime(2026, 11, 2, 16, 25, tzinfo=dt.timezone.utc)
END = dt.datetime(2026, 11, 5, 16, 0, tzinfo=dt.timezone.utc)
POLICY_FILES = {
    ".github/workflows/ci.yml", ".github/security/alpha8_audit.py",
    ".github/security/test_alpha8_audit.py", ".github/security/alpha8-decision.md",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def verify_payload(archive, head):
    files = {}
    provenance = None
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            root, sep, name = member.name.partition("/")
            require(member.isfile() and sep and root == PACKAGE and name and name not in files,
                    "unexpected or duplicate package member")
            data = tar.extractfile(member).read()
            if name == ".cargo_vcs_info.json":
                require(provenance is None, "duplicate VCS provenance")
                provenance = json.loads(data)
            else:
                files[name] = [member.mode, sha(data)]
    require(provenance == {"git": {"sha1": head}, "path_in_vcs": ""}, "unexpected package provenance")
    require(sha(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()) == PAYLOAD,
            "packaged payload differs from the approved alpha")


def findings(report, inactive):
    summary = report["vulnerabilities"]
    entries = summary["list"]
    require(summary["count"] == len(entries) and summary["found"] == bool(entries), "incomplete audit findings")
    rows = [(v["advisory"]["id"], *(v["package"][key] for key in ("name", "version", "source", "checksum")))
            for v in entries]
    require(len(rows) == len(set(rows)), "duplicate vulnerability findings")
    require(not set(rows) - {BIGINT, inactive}, "unaccepted vulnerability in the unfiltered audit")
    return BIGINT in rows


def qualify(archive, now, head, changes):
    require(START <= now < END, "alpha qualification acceptance is outside its approved time window")
    require(not set(changes) - POLICY_FILES, "source changed outside qualification policy")
    require(sha((ROOT / "Cargo.lock").read_bytes()) == LOCK, "lockfile differs from the approved alpha")
    verify_payload(archive, head)


def run():
    # A temporary working directory prevents the project's inactive-entry ignore
    # from hiding findings in the retained, freshly fetched audit report.
    version = subprocess.check_output(["cargo-audit", "--version"], text=True).strip()
    require(version == "cargo-audit 0.22.2", "unexpected cargo-audit version")
    with tempfile.TemporaryDirectory() as work:
        audit = subprocess.run(["cargo-audit", "audit", "--json", "--file", str(ROOT / "Cargo.lock")],
                               cwd=work, capture_output=True, text=True)
    require(audit.returncode in (0, 1), "cargo-audit failed to run")
    report = json.loads(audit.stdout)
    output = ROOT / "target" / "alpha8-audit.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as stream:
            stream.write("### Unfiltered dependency audit\n\n```json\n" + output.read_text() + "```\n")
    require(re.fullmatch(r"[0-9a-f]{40}", report["database"].get("last-commit", "")),
            "fresh advisory database identity unavailable")
    for entry in report["vulnerabilities"]["list"]:
        print(entry["advisory"]["id"], entry["package"]["name"], entry["package"]["version"], flush=True)
    # All existing reachability and warning scopes remain independently enforced.
    subprocess.run(["bash", "scripts/check-security-exceptions.sh"], cwd=ROOT, check=True)
    import tomllib
    locked = tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
    tracing = next(p for p in locked if p["name"] == "tracing-subscriber" and p["version"] == "0.2.25")
    inactive = ("RUSTSEC-2025-0055", tracing["name"], tracing["version"], tracing["source"], tracing["checksum"])
    has_bigint = findings(report, inactive)
    is_candidate = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"] == "0.3.1-alpha.8"
    if is_candidate or has_bigint:
        require(has_bigint, "approved finding absent from the unfiltered audit; review its disposition")
        subprocess.run(["git", "merge-base", "--is-ancestor", BASE, "HEAD"], cwd=ROOT, check=True)
        subprocess.run(["git", "diff", "--quiet", "HEAD"], cwd=ROOT, check=True)
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        changes = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", BASE, "HEAD"],
                                          cwd=ROOT).decode().split("\0")
        archive = ROOT / "target" / "package" / (PACKAGE + ".crate")
        qualify(archive, dt.datetime.now(dt.timezone.utc), head, filter(None, changes))
        print(f"RUSTSEC-2026-0387 accepted only for {PACKAGE} qualification until {END.isoformat()}; "
              f"archive SHA-256 {sha(archive.read_bytes())}. Publication and downstream use are separate decisions.")


if __name__ == "__main__":
    try:
        run()
    except (ValueError, KeyError, OSError, StopIteration, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit(f"Qualification refused: {error}")
