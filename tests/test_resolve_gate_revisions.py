#!/usr/bin/env python3
"""Contract tests for the instruction-gate revision policy (issue #571)."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "resolve_gate_revisions", ROOT / "tools" / "resolve_gate_revisions.py"
)
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)

BASE = "a" * 40
HEAD = "b" * 40
PARENT = "c" * 40


def pr_payload(base=BASE, head=HEAD):
    return {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}


class ResolveTests(unittest.TestCase):
    def test_pull_request_uses_base_and_head(self):
        self.assertEqual(
            resolver.resolve("pull_request", pr_payload(), "0" * 40), (BASE, HEAD)
        )

    def test_pull_request_rejects_missing_or_equal_shas(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("pull_request", {}, HEAD)
        with self.assertRaisesRegex(ValueError, "must differ"):
            resolver.resolve(
                "pull_request", pr_payload(base=HEAD, head=HEAD), HEAD
            )

    def test_push_uses_before_and_sha(self):
        self.assertEqual(
            resolver.resolve("push", {"before": BASE}, HEAD), (BASE, HEAD)
        )

    def test_push_zero_before_falls_back_to_parent(self):
        calls = []
        base, head = resolver.resolve(
            "push",
            {"before": "0" * 40},
            HEAD,
            parent_of=lambda sha: (calls.append(sha), PARENT)[1],
        )
        self.assertEqual((base, head), (PARENT, HEAD))
        self.assertEqual(calls, [HEAD])

    def test_push_rejects_missing_head(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("push", {"before": BASE}, "")

    def test_dispatch_inputs_win(self):
        payload = {"inputs": {"base_sha": BASE, "head_sha": HEAD}}
        self.assertEqual(
            resolver.resolve("workflow_dispatch", payload, PARENT), (BASE, HEAD)
        )

    def test_dispatch_without_inputs_uses_parent(self):
        base, head = resolver.resolve(
            "workflow_dispatch", {}, HEAD,
            parent_of=lambda sha: PARENT,
        )
        self.assertEqual((base, head), (PARENT, HEAD))

    def test_dispatch_partial_inputs_are_incomplete(self):
        payload = {"inputs": {"base_sha": BASE}}
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve("workflow_dispatch", payload, HEAD)

    def test_explicit_inputs_override_any_event(self):
        base, head = resolver.resolve(
            "push", {"before": PARENT}, HEAD,
            base_input=BASE, head_input=HEAD,
        )
        self.assertEqual((base, head), (BASE, HEAD))
        with self.assertRaisesRegex(ValueError, "incomplete"):
            resolver.resolve(
                "push", {"before": BASE}, HEAD,
                base_input=BASE, head_input="short",
            )

    def test_unknown_event_needs_explicit_inputs(self):
        with self.assertRaisesRegex(ValueError, "explicit base/head"):
            resolver.resolve("schedule", {}, HEAD)
        self.assertEqual(
            resolver.resolve(
                "schedule", {}, HEAD, base_input=BASE, head_input=HEAD
            ),
            (BASE, HEAD),
        )

    def test_cli_prints_both_shas(self):
        with tempfile.TemporaryDirectory(prefix="gate-revs-") as tmp:
            event = Path(tmp) / "event.json"
            event.write_text(json.dumps(pr_payload()), encoding="utf-8")
            with tempfile.TemporaryDirectory(prefix="gate-out-") as out:
                output = Path(out) / "github_output"
                import os

                env = dict(os.environ)
                env["GITHUB_OUTPUT"] = str(output)
                code = None
                old = dict(os.environ)
                try:
                    os.environ["GITHUB_OUTPUT"] = str(output)
                    code = resolver.main([
                        "--event-name", "pull_request",
                        "--event-path", str(event),
                        "--sha", HEAD,
                    ])
                finally:
                    os.environ.clear()
                    os.environ.update(old)
                _ = env
                self.assertEqual(code, 0)
                written = output.read_text(encoding="utf-8")
            self.assertIn(f"base_sha={BASE}", written)
            self.assertIn(f"head_sha={HEAD}", written)

    def test_cli_reports_incomplete_without_payload(self):
        # Hermetic against ambient GitHub Actions state: runners always
        # export GITHUB_EVENT_PATH, which main() honors when --event-path
        # is omitted. Without scrubbing, a valid ambient payload turns
        # this no-payload case exit 0 instead of the expected 2.
        import os

        old = dict(os.environ)
        try:
            for key in (
                "GITHUB_EVENT_NAME",
                "GITHUB_EVENT_PATH",
                "GITHUB_SHA",
                "GITHUB_OUTPUT",
            ):
                os.environ.pop(key, None)
            self.assertEqual(
                resolver.main(["--event-name", "pull_request", "--sha", HEAD]),
                2,
            )
        finally:
            os.environ.clear()
            os.environ.update(old)

    def test_cli_uses_ambient_event_path_when_no_explicit_path(self):
        # Regression for the GitHub Actions interaction above: with no
        # --event-path flag, the resolver must honor the ambient
        # GITHUB_EVENT_PATH payload (the production path used by the
        # "Resolve gate revisions" step). Fail-closed handling is
        # unchanged: this only provides the payload, it never synthesizes
        # revisions.
        with tempfile.TemporaryDirectory(prefix="gate-ambient-") as tmp:
            import os

            event = Path(tmp) / "event.json"
            event.write_text(json.dumps(pr_payload()), encoding="utf-8")
            output = Path(tmp) / "github_output"
            old = dict(os.environ)
            try:
                os.environ.pop("GITHUB_EVENT_NAME", None)
                os.environ["GITHUB_EVENT_PATH"] = str(event)
                os.environ.pop("GITHUB_SHA", None)
                os.environ["GITHUB_OUTPUT"] = str(output)
                code = resolver.main(
                    ["--event-name", "pull_request", "--sha", HEAD]
                )
            finally:
                os.environ.clear()
                os.environ.update(old)
            self.assertEqual(code, 0)
            written = output.read_text(encoding="utf-8")
            self.assertIn(f"base_sha={BASE}", written)
            self.assertIn(f"head_sha={HEAD}", written)


class FetchOrderingTests(unittest.TestCase):
    """Fork-PR heads live on pull/N/head, not origin: refetch first (issue #571).

    The "Fetch gate revisions" step must fetch the pull-request head ref
    before the required per-SHA fetches, or a fork-PR head SHA fails the
    job before the ref that provides it runs. Static checks only:
    no network, no checkout, no untrusted code execution.
    """

    WORKFLOW = ROOT / ".github" / "workflows" / "bash-compare.yml"

    def fetch_step(self):
        text = self.WORKFLOW.read_text(encoding="utf-8")
        marker = "Fetch gate revisions"
        start = text.index(marker)
        # Bound the step at the next "- name:" so later build steps cannot
        # satisfy the ordering assertions by accident.
        following = text.find("\n      - name:", start + len(marker))
        return text[start:following if following != -1 else len(text)]

    def test_fork_head_refetch_precedes_required_sha_fetch(self):
        step = self.fetch_step()
        pr_ref = step.index("pull/${{ github.event.pull_request.number }}/head")
        base_fetch = step.index('origin "$BASE_SHA"')
        head_fetch = step.index('origin "$HEAD_SHA"')
        self.assertLess(
            pr_ref, base_fetch,
            "pull/N/head refetch must run before the required SHA fetches "
            "so a fork-PR head SHA is present when it is fetched",
        )
        self.assertLess(pr_ref, head_fetch)

    def test_required_sha_fetches_stay_fail_closed_and_split(self):
        step = self.fetch_step()
        base_line = next(
            line for line in step.splitlines()
            if 'origin "$BASE_SHA"' in line
        )
        head_line = next(
            line for line in step.splitlines()
            if 'origin "$HEAD_SHA"' in line
        )
        # Per-SHA fetches: a denial names only the missing SHA, and one
        # SHA's denial cannot mask the other.
        self.assertNotEqual(base_line, head_line)
        self.assertNotIn("|| true", base_line)
        self.assertNotIn("|| true", head_line)
        self.assertIn("cannot fetch base revision $BASE_SHA", step)
        self.assertIn("cannot fetch head revision $HEAD_SHA", step)
        self.assertIn("exit 2", step)

    def test_fork_refetch_is_guarded_best_effort_and_numeric(self):
        step = self.fetch_step()
        refetch_line = next(
            line for line in step.splitlines()
            if "git fetch" in line and "pull/" in line
        )
        self.assertIn("|| true", refetch_line)
        self.assertIn('GITHUB_EVENT_NAME" = pull_request', step)
        self.assertIn("github.event.pull_request.number", step)
        # PR identity is GitHub-generated, but the number is interpolated
        # into a refspec: reject anything but decimal digits, fail-closed.
        self.assertIn("*[!0-9]*", step)
        self.assertIn("not numeric", step)

    def test_fetched_revisions_are_verified_before_use(self):
        step = self.fetch_step()
        head_fetch = step.index('origin "$HEAD_SHA"')
        base_check = step.index('cat-file -e "$BASE_SHA^{commit}"')
        head_check = step.index('cat-file -e "$HEAD_SHA^{commit}"')
        self.assertLess(head_fetch, base_check)
        self.assertLess(head_fetch, head_check)


class ForkHeadFixtureTests(unittest.TestCase):
    """Behavioral fork-head fetching against a local restricted server.

    Stock-git upload-pack rules are enforced through an
    ``ext::git upload-pack <server>`` remote: plain-path and ``file://``
    transports bypass ``uploadpack.hideRefs`` and the ``allow*SHA1InWant``
    denials, so they cannot reproduce the failure. Each fixture builds an
    upstream bare repo (``main`` plus a fork-only ``refs/pull/123/head``),
    denies SHA-want, and clones only ``main`` into a runner, mirroring a
    PR runner whose default refspec omits pull refs. No network, no daemon.
    """

    def _run(self, *args, cwd=None, env=None):
        import os
        import subprocess

        merged = dict(os.environ)
        merged["GIT_ALLOW_PROTOCOL"] = "ext:file"
        merged["GIT_CONFIG_COUNT"] = "2"
        merged["GIT_CONFIG_KEY_0"] = "protocol.ext.allow"
        merged["GIT_CONFIG_VALUE_0"] = "always"
        merged["GIT_CONFIG_KEY_1"] = "protocol.file.allow"
        merged["GIT_CONFIG_VALUE_1"] = "always"
        if env:
            merged.update(env)
        return subprocess.run(
            list(args),
            capture_output=True,
            text=True,
            cwd=str(cwd) if cwd is not None else None,
            env=merged,
        )

    def _make_fixture(self, hide_pull_refs):
        import subprocess
        from pathlib import Path

        tmp = Path(self._tmpdir())
        srv = tmp / "upstream.git"
        seed = tmp / "seed"
        runner = tmp / "runner"
        self._run("git", "init", "--bare", "-q", str(srv), cwd=tmp).check_returncode()
        self._run("git", "init", "-q", "-b", "main", str(seed), cwd=tmp).check_returncode()
        self._run("git", "-C", str(seed), "config", "user.email", "gate@test").check_returncode()
        self._run("git", "-C", str(seed), "config", "user.name", "gate").check_returncode()
        (seed / "file.txt").write_text("base\n", encoding="utf-8")
        self._run("git", "-C", str(seed), "add", "file.txt").check_returncode()
        self._run("git", "-C", str(seed), "commit", "-qm", "base").check_returncode()
        base = self._run("git", "-C", str(seed), "rev-parse", "HEAD").stdout.strip()
        self._run(
            "git", "-C", str(seed), "push", "-q", str(srv), "HEAD:refs/heads/main"
        ).check_returncode()
        (seed / "file.txt").write_text("base\nfork\n", encoding="utf-8")
        self._run("git", "-C", str(seed), "add", "file.txt").check_returncode()
        self._run("git", "-C", str(seed), "commit", "-qm", "fork").check_returncode()
        fork = self._run("git", "-C", str(seed), "rev-parse", "HEAD").stdout.strip()
        self._run(
            "git", "-C", str(seed), "push", "-q", str(srv), "HEAD:refs/pull/123/head"
        ).check_returncode()
        for key in (
            "allowTipSHA1InWant",
            "allowReachableSHA1InWant",
            "allowAnySHA1InWant",
        ):
            self._run(
                "git", "--git-dir", str(srv), "config", f"uploadpack.{key}", "false"
            ).check_returncode()
        if hide_pull_refs:
            self._run(
                "git", "--git-dir", str(srv), "config",
                "uploadpack.hideRefs", "refs/pull/",
            ).check_returncode()
        self._run("git", "init", "-q", "-b", "main", str(runner), cwd=tmp).check_returncode()
        self._run(
            "git", "-C", str(runner), "remote", "add", "origin",
            f"ext::git upload-pack {srv}",
        ).check_returncode()
        fetched = self._run(
            "git", "-C", str(runner), "fetch", "--no-tags", "origin",
            "refs/heads/main:refs/remotes/origin/main",
        )
        self.assertEqual(fetched.returncode, 0, fetched.stderr)
        self.assertNotEqual(
            self._run("git", "-C", str(runner), "cat-file", "-e", fork).returncode, 0,
            "runner must start without the fork object",
        )
        return {"tmp": tmp, "srv": srv, "runner": runner, "base": base, "fork": fork}

    def _tmpdir(self):
        import tempfile

        tmp = tempfile.mkdtemp(prefix="gate-fork-")
        self.addCleanup(__import__("shutil").rmtree, tmp, True)
        return tmp

    def test_required_sha_fetch_fails_when_head_absent_and_denied(self):
        """Unadvertised SHA-want denied while the object is not yet local."""
        fix = self._make_fixture(hide_pull_refs=True)
        fetched = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin", fix["fork"]
        )
        self.assertNotEqual(fetched.returncode, 0)
        self.assertIn("unadvertised object", fetched.stderr)
        # The advertised base still fetches: the gate is fail-closed per SHA.
        base_fetch = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin", fix["base"]
        )
        self.assertEqual(base_fetch.returncode, 0, base_fetch.stderr)

    def test_stock_git_rejects_hidden_pull_ref_by_name(self):
        """Pin the GitHub dependency: stock hideRefs rejects the named refetch."""
        fix = self._make_fixture(hide_pull_refs=True)
        refetch = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin",
            "pull/123/head:refs/remotes/pull/123/head",
        )
        self.assertNotEqual(refetch.returncode, 0)
        self.assertIn("couldn't find remote ref", refetch.stderr)

    def test_pull_refetch_first_then_sha_fetch_verifies(self):
        """Refetch-first provides the head; the required SHA fetch then verifies."""
        fix = self._make_fixture(hide_pull_refs=True)
        # Required-first fails before any refetch can help.
        denied = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin", fix["fork"]
        )
        self.assertNotEqual(denied.returncode, 0)
        # Simulate GitHub serving pull/N/head by explicit name: unhide it.
        unhide = self._run(
            "git", "--git-dir", str(fix["srv"]), "config", "--unset",
            "uploadpack.hideRefs",
        )
        self.assertEqual(unhide.returncode, 0, unhide.stderr)
        refetch = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin",
            "pull/123/head:refs/remotes/pull/123/head",
        )
        self.assertEqual(refetch.returncode, 0, refetch.stderr)
        self.assertEqual(
            self._run(
                "git", "-C", str(fix["runner"]), "cat-file", "-e",
                f"{fix['fork']}^{{commit}}",
            ).returncode, 0,
        )
        # Restrict again: the required SHA fetch now succeeds from the local
        # object, doubling as fail-closed verification of the refetch.
        rehide = self._run(
            "git", "--git-dir", str(fix["srv"]), "config",
            "uploadpack.hideRefs", "refs/pull/",
        )
        self.assertEqual(rehide.returncode, 0, rehide.stderr)
        verify = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin", fix["fork"]
        )
        self.assertEqual(verify.returncode, 0, verify.stderr)

    def test_sha_fetch_short_circuits_when_object_already_local(self):
        """Once local, the SHA fetch cannot fail for lack of the object."""
        fix = self._make_fixture(hide_pull_refs=False)
        refetch = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags", "origin",
            "pull/123/head:refs/remotes/pull/123/head",
        )
        self.assertEqual(refetch.returncode, 0, refetch.stderr)
        bogus = self._run(
            "git", "-C", str(fix["runner"]), "fetch", "--no-tags",
            "ext::git upload-pack /nonexistent-571.git", fix["fork"],
        )
        self.assertEqual(bogus.returncode, 0, bogus.stderr)


if __name__ == "__main__":
    unittest.main()
