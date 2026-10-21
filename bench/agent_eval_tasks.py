"""Pinned task preparation and independent scoring, without provider calls."""
from dataclasses import dataclass
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

from eval_files import source_paths


def snapshot(root):
    return {p.relative_to(root).as_posix(): (
        b"link:" + str(p.readlink()).encode() if p.is_symlink() else b"file:" + p.read_bytes()
    ) for p in source_paths(root)}


def digest(files):
    result = hashlib.sha256()
    for name, body in sorted(files.items()):
        result.update(name.encode() + b"\0" + hashlib.sha256(body).digest())
    return result.hexdigest()


def relative_path(value):
    if not isinstance(value, str) or not value or "\\" in value:
        raise ValueError(f"expected a relative POSIX path: {value!r}")
    path = Path(value)
    if path.is_absolute() or ".." in path.parts:
        raise ValueError(f"path escapes task resources: {value}")
    return value


@dataclass(frozen=True)
class AnswerOracle:
    expected: object
    evidence: dict


@dataclass(frozen=True)
class EditOracle:
    checker: Path
    solution: Path
    allowed_paths: tuple


@dataclass(frozen=True)
class Task:
    id: str
    kind: str
    repo: str
    commit: str
    prompt: str
    oracle: AnswerOracle | EditOracle


@dataclass
class PreparedTask:
    task: Task
    workspace: Path
    pristine: dict

    @property
    def source_sha256(self):
        return digest(self.pristine)

    def validate(self, final_text):
        try:
            final = json.loads(final_text)
        except (ValueError, TypeError):
            return {"status": "fail", "passed": False, "reason": "final answer was not JSON"}
        actual = snapshot(self.workspace)
        changed = sorted(name for name in self.pristine.keys() | actual.keys()
                         if self.pristine.get(name) != actual.get(name))
        oracle = self.task.oracle
        if isinstance(oracle, AnswerOracle):
            passed = (not changed and isinstance(final, dict) and "answer" in final
                      and json.dumps(final.get("answer"), sort_keys=True)
                      == json.dumps(oracle.expected, sort_keys=True))
            return {"status": "pass" if passed else "fail", "passed": passed,
                    "observed": final, "changed_files": changed}
        if not isinstance(final, dict) or final.get("status") != "done":
            return {"status": "fail", "passed": False, "reason": "missing final status done"}
        if set(changed) != set(oracle.allowed_paths):
            return {"status": "fail", "passed": False, "reason": "unexpected changed files",
                    "changed_files": changed}
        try:
            with tempfile.TemporaryDirectory(prefix="codeq-oracle-baseline-") as temp:
                baseline = Path(temp) / "source.json"
                baseline.write_text(json.dumps({
                    name: self.pristine[name].removeprefix(b"file:").decode()
                    for name in oracle.allowed_paths if name in self.pristine
                }))
                result = subprocess.run(
                    [sys.executable, "-I", "-B", str(oracle.checker), str(self.workspace), str(baseline)],
                    capture_output=True, text=True, timeout=30,
                )
        except subprocess.TimeoutExpired:
            return {"status": "fail", "passed": False, "reason": "oracle timed out"}
        passed = result.returncode == 0
        return {"status": "pass" if passed else "fail", "passed": passed,
                "changed_files": changed, "validation_exit_code": result.returncode,
                "validation_stdout": result.stdout, "validation_stderr": result.stderr}


class TaskSuite:
    def __init__(self, manifest, corpus):
        self.manifest = Path(manifest).resolve()
        self.corpus = Path(corpus).resolve()
        raw = self.manifest.read_bytes()
        self.sha256 = hashlib.sha256(raw).hexdigest()
        spec = json.loads(raw)
        if spec.get("version") != 1 or not isinstance(spec.get("tasks"), list):
            raise ValueError("expected a version 1 task manifest")
        self.tasks = []
        seen = set()
        for item in spec["tasks"]:
            ident, repo, commit = item["id"], item["repo"], item["commit"]
            if (not re.fullmatch(r"[a-z0-9-]+", ident) or ident in seen
                    or not re.fullmatch(r"[a-z0-9-]+", repo)
                    or not re.fullmatch(r"[0-9a-f]{40}", commit)):
                raise ValueError(f"invalid or repeated task identity: {ident}")
            seen.add(ident)
            oracle = item["oracle"]
            if item["kind"] in ("answer", "discovery", "tests"):
                evidence = oracle["evidence"]
                for path, sha in evidence.items():
                    relative_path(path)
                    if not re.fullmatch(r"[0-9a-f]{64}", sha):
                        raise ValueError(f"invalid evidence hash: {path}")
                if not evidence:
                    raise ValueError(f"missing answer evidence: {ident}")
                parsed = AnswerOracle(oracle["expected"], evidence)
            elif item["kind"] == "edit":
                def resource(key):
                    path = (self.manifest.parent / relative_path(oracle[key])).resolve()
                    if not path.is_relative_to(self.manifest.parent) or not path.is_file():
                        raise ValueError(f"missing or escaping oracle resource: {path}")
                    return path
                allowed = tuple(relative_path(p) for p in oracle["allowed_paths"])
                if not allowed or len(set(allowed)) != len(allowed):
                    raise ValueError(f"invalid edit paths: {ident}")
                parsed = EditOracle(resource("checker"), resource("solution"), allowed)
            else:
                raise ValueError(f"unknown task kind: {item['kind']}")
            if not isinstance(item["prompt"], str) or not item["prompt"].strip():
                raise ValueError(f"missing prompt: {ident}")
            self.tasks.append(Task(ident, item["kind"], repo, commit, item["prompt"], parsed))
        if not self.tasks:
            raise ValueError("task manifest is empty")

    def prepare(self, task, destination):
        repo = self.corpus / task.repo
        revision = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", f"{task.commit}^{{commit}}"],
            capture_output=True, text=True, check=True, timeout=30,
        ).stdout.strip()
        if revision != task.commit:
            raise ValueError(f"wrong source revision for {task.id}: {revision}")
        archive = subprocess.run(
            ["git", "-C", str(repo), "archive", "--format=tar", task.commit],
            capture_output=True, check=True, timeout=30,
        ).stdout
        destination.mkdir(parents=True)
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            source.extractall(destination, filter="data")
        pristine = snapshot(destination)
        if isinstance(task.oracle, AnswerOracle):
            for name, expected in task.oracle.evidence.items():
                actual = hashlib.sha256((destination / name).read_bytes()).hexdigest()
                if actual != expected:
                    raise ValueError(f"stale answer evidence for {task.id}: {name}")
        return PreparedTask(task, destination, pristine)

    def self_check(self):
        reports = []
        with tempfile.TemporaryDirectory(prefix="codeq-oracle-check-") as temp:
            for task in self.tasks:
                prepared = self.prepare(task, Path(temp) / task.id)
                if isinstance(task.oracle, EditOracle):
                    # Run the behavioral oracle directly so a no-op edit cannot
                    # fail solely because the allowed files were not changed.
                    command = [sys.executable, "-I", "-B", str(task.oracle.checker),
                               str(prepared.workspace)]
                    baseline = subprocess.run(command, capture_output=True, text=True, timeout=30)
                    if baseline.returncode == 0:
                        raise ValueError(f"edit baseline already passes: {task.id}")
                    subprocess.run(["git", "apply", str(task.oracle.solution)],
                                   cwd=prepared.workspace, check=True, capture_output=True, timeout=30)
                    verdict = prepared.validate('{"status":"done"}')
                    if not verdict["passed"]:
                        raise ValueError(f"known-good solution failed: {task.id}: {verdict}")
                    reports.append({"task": task.id, "baseline_failed": True,
                                    "solution_passed": True, "source_sha256": prepared.source_sha256})
                else:
                    answer = json.dumps({"answer": task.oracle.expected})
                    if not prepared.validate(answer)["passed"]:
                        raise ValueError(f"answer oracle failed: {task.id}")
                    reports.append({"task": task.id, "evidence_verified": True,
                                    "source_sha256": prepared.source_sha256})
        return reports
