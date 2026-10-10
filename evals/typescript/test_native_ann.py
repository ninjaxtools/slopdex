"""Optional real-CLI regression: SLOPDEX_EVAL_CHECK_ANN=1 python3 -m unittest
discover -s evals/typescript -p test_native_ann.py -v.

Only a tiny local Git repository and a stdlib localhost embeddings fixture are
used. The installed slopdex executable is required when explicitly enabled.
"""

from contextlib import closing
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

import run as runner


@unittest.skipUnless(os.environ.get("SLOPDEX_EVAL_CHECK_ANN") == "1",
                     "Optional installed-slopdex native ANN regression")
class NativeAnnCompatibilityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = runner.executable("slopdex")
        temporary = tempfile.TemporaryDirectory(prefix="typescript-native-ann-")
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name).resolve()
        home = cls.root / "home"
        home.mkdir()
        # No inherited provider credentials, proxy settings, or user config.
        cls.environment = {
            "PATH": os.environ.get("PATH", os.defpath),
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_CACHE_HOME": str(home / "cache"),
            "XDG_DATA_HOME": str(home / "data"),
            "XDG_STATE_HOME": str(home / "state"),
            "NO_PROXY": "127.0.0.1,localhost",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        original_command, original_process = runner.command, runner.logged_process

        def isolated_command(argv, *, cwd=runner.ROOT, env=None, timeout=120):
            return original_command(argv, cwd=cwd, env=cls.environment, timeout=timeout)

        def isolated_process(argv, cwd, env, directory, timeout):
            result = original_process(argv, cwd, cls.environment, directory, timeout)
            if result[0] or result[1]:
                raise AssertionError(f"Fixture command failed: {argv!r}\n{(directory / 'stderr.log').read_text()}\nRequests: {cls.requests!r}")
            return result

        for name, implementation in (("command", isolated_command), ("logged_process", isolated_process)):
            isolation = patch.object(runner, name, side_effect=implementation)
            isolation.start()
            cls.addClassCleanup(isolation.stop)
        cls.requests = []
        model, api_key, dimensions = "native-ann-fixture", "fixture-key", 4

        class EmbeddingsHandler(BaseHTTPRequestHandler):
            def log_message(self, format, *args):
                pass

            def do_GET(self):
                cls.requests.append((self.path, None, None))
                self.send_error(404)

            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                authorization = self.headers.get("Authorization")
                cls.requests.append((self.path, authorization, payload))
                inputs = payload.get("input")
                if (self.path != "/v1/embeddings" or authorization != f"Bearer {api_key}"
                    or payload.get("model") != model or payload.get("dimensions") != dimensions
                    or not isinstance(inputs, list) or not inputs
                    or not all(isinstance(text, str) and text for text in inputs)):
                    self.send_error(400, "Unexpected fixture embedding request")
                    return
                data = []
                for index, text in enumerate(inputs):
                    values = hashlib.sha256(text.encode()).digest()[:dimensions]
                    data.append({"object": "embedding", "index": index,
                                 "embedding": [(value + 1) / 256 for value in values]})
                body = json.dumps({"object": "list", "model": model, "data": data,
                                   "usage": {"prompt_tokens": len(inputs),
                                             "total_tokens": len(inputs)}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), EmbeddingsHandler)
        thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        thread.start()

        def stop_server():
            if thread.is_alive():
                cls.server.shutdown()
                thread.join(timeout=5)
            cls.server.server_close()

        cls.addClassCleanup(stop_server)
        cls.source = cls.root / "source-repo"
        cls.source.mkdir()
        (cls.source / "source.go").write_text(
            "package fixture\n\n"
            "// alpha increments a number.\n"
            "func alpha(value int) int { return value + 1 }\n\n"
            "// beta doubles a number.\n"
            "func beta(value int) int { return value * 2 }\n\n"
            "// gamma composes the two transformations.\n"
            "func gamma(value int) int { return beta(alpha(value)) }\n\n"
            "// \".\" | \"@\" | \"#\"\n"
            "const separator = 0\n"
        )
        runner.command(["git", "init", "-q", str(cls.source)], cwd=cls.root)
        runner.command(["git", "add", "source.go"], cwd=cls.source)
        runner.command(["git", "-c", "user.name=ANN Fixture", "-c",
                        "user.email=ann@example.invalid", "commit", "-qm", "fixture"],
                       cwd=cls.source)
        cls.commit = runner.command(["git", "rev-parse", "HEAD"], cwd=cls.source)
        cls.config = {
            "provider": "openai", "model": model, "dimensions": dimensions,
            "embeddingBaseUrl": f"http://127.0.0.1:{cls.server.server_port}/v1",
            "embeddingApiKey": api_key, "artifactCachePath": str(cls.root / "artifacts.sqlite"),
            "rerankingEnabled": False, "include": ["*.go"],
            "providerMaxRetries": 0, "providerTimeoutMs": 2000,
        }
        # Build only the symbol map snapshot; no semantic content preparation.
        symbol_config = {"slopdex": dict(cls.config), "index_timeout_seconds": 30}
        tasks = [{"targets": [{"path": "source.go", "symbol": name} for name in ("alpha", "beta", "gamma")]}]
        with patch.object(runner, "SUBMODULE", cls.source):
            cls.symbol_cache, cls.symbol_metadata = runner.prepare_map_index(
                symbol_config, cls.commit, cls.root / "symbol-cache", cls.binary, tasks, symbols=True)
        cls.symbol_warm_requests = list(cls.requests)
        warm_trial = cls.root / "symbol-preflight"
        warm_trial.mkdir()
        copied_index = warm_trial / "index.sqlite"
        runner.copy_index(cls.symbol_cache / "index.sqlite", copied_index, cls.source, cls.commit, symbol_only=True)
        runner.verify_native_ann_reuse(cls.binary, cls.source, cls.symbol_cache / "config.json", copied_index,
                                       warm_trial, 30, symbol_only=True)
        if cls.requests != cls.symbol_warm_requests:
            raise AssertionError("Prepared map snapshot required provider calls during preflight")
        runner.command([cls.binary, "--root", str(cls.source), "--config", str(cls.symbol_cache / "config.json"),
                        "--index", str(cls.symbol_cache / "index.sqlite"), "--no-reindex", "map", "--private",
                        "-q", "alpha", "--symbol-threshold", "0.99"], cwd=cls.source)
        cls.symbol_requests = list(cls.requests)
        stop_server()
        # Preserve the endpoint/profile, but make accidental provider use fail
        # against a stopped localhost server without any available credentials.
        cls.config.pop("embeddingApiKey")
        symbol_settings = runner.read_json(cls.symbol_cache / "config.json")
        symbol_settings.pop("embeddingApiKey")
        runner.write_json(cls.symbol_cache / "config.json", symbol_settings)
        cls.index = cls.symbol_cache / "index.sqlite"
        cls.config_path = cls.symbol_cache / "config.json"
        cls.source_snapshot = runner.native_ann_snapshot(cls.index, symbol_only=True)
        cls.source_files = cls.file_snapshot(cls.index, cls.source_snapshot)
        cls.source_metadata = cls.sqlite_metadata(cls.index)

    @staticmethod
    def sqlite_metadata(index):
        with closing(sqlite3.connect(f"{index.as_uri()}?mode=ro", uri=True)) as db:
            return dict(db.execute("SELECT key, value FROM metadata"))

    @staticmethod
    def file_snapshot(index, snapshot):
        paths = {"": index, **runner.native_ann_paths(index, snapshot)}
        return {key: (runner.file_digest(path), path.stat().st_mtime_ns) for key, path in paths.items()}

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="trial-", dir=self.root)
        self.addCleanup(temporary.cleanup)
        self.trial = Path(temporary.name).resolve()
        self.workspace = self.trial / "relocated-repo"
        runner.command(["git", "clone", "--quiet", "--no-hardlinks", str(self.source),
                        str(self.workspace)], cwd=self.trial)
        self.assertNotEqual(self.workspace.resolve(), self.source.resolve())
        self.assertEqual(runner.command(["git", "rev-parse", "HEAD"], cwd=self.workspace), self.commit)
        self.assertEqual(runner.command(["git", "status", "--porcelain"], cwd=self.workspace), "")

    def test_symbol_map_preparation_and_writable_reuse_after_clone_relocation(self):
        self.assertGreater(len(self.symbol_warm_requests), 0)
        inputs = [text for path, authorization, payload in self.symbol_warm_requests for text in payload["input"]]
        self.assertIn("slopdex eval warmup", inputs)
        self.assertEqual(self.symbol_metadata["identity"]["kind"], "map-symbols")
        task = {"targets": [{"path": "source.go", "symbol": name}
                            for name in ("alpha", "beta", "gamma")]}
        self.assertEqual(runner.index_coverage(self.index, [task], semantic=False),
                          {"validated_targets": 3, "parser_diagnostics": 0})
        self.assertEqual(Path(self.symbol_metadata["status"]["rootDir"]).resolve(), self.source.resolve())
        saved = runner.read_json(self.symbol_cache / "manifest.json")
        self.assertEqual(saved, {key: value for key, value in self.symbol_metadata.items() if key != "coverage"})
        self.assertTrue(runner.native_ann_ready(self.index, self.symbol_metadata, symbol_only=True))
        self.assertEqual(set(self.source_snapshot["artifacts"]), {".symbols.shared.json"})

        destination = self.trial / "index.sqlite"
        runner.copy_index(self.index, destination, self.workspace, self.commit, symbol_only=True)
        self.assertEqual(runner.native_ann_snapshot(destination, symbol_only=True), self.source_snapshot)
        before = self.file_snapshot(destination, self.source_snapshot)
        for key in runner.native_ann_paths(destination, self.source_snapshot):
            self.assertEqual(before[key], self.source_files[key], key)
        copied_metadata = self.sqlite_metadata(destination)
        identity = json.loads(copied_metadata["identity"])
        self.assertEqual(Path(identity["root"]).resolve(), self.workspace.resolve())
        self.assertEqual(copied_metadata["checkpoint"], self.commit)
        self.assertEqual(copied_metadata["generation"], self.source_metadata["generation"])
        self.assertNotIn("embeddingApiKey", runner.read_json(self.config_path))
        self.assertFalse(any("API_KEY" in key for key in self.environment))

        result = runner.verify_native_ann_reuse(self.binary, self.workspace, self.config_path,
                                              destination, self.trial, 30, symbol_only=True)
        self.assertTrue(result["validated"])
        self.assertEqual({key: value for key, value in result["snapshot"].items() if key != "generation"},
                         {key: value for key, value in self.source_snapshot.items() if key != "generation"})
        self.assertEqual(runner.read_json(self.trial / "ann-validation.json"), result)
        after = self.file_snapshot(destination, self.source_snapshot)
        for key in runner.native_ann_paths(destination, self.source_snapshot):
            self.assertEqual(after[key], before[key], f"Native artifact rebuilt: {key}")
        self.assertEqual(self.sqlite_metadata(destination)["generation"], str(result["snapshot"]["generation"]))
        self.assertTrue(runner.native_ann_ready(destination, {"native_ann": {**result["snapshot"], "warmup_version": 2}}, symbol_only=True))
        self.assertEqual(self.requests, self.symbol_requests)
        for logs in (self.trial / "ann-validation",):
            for name in ("stdout.jsonl", "stderr.log"):
                self.assertNotIn("external model call", (logs / name).read_text())
        self.assertEqual(runner.native_ann_snapshot(self.index, symbol_only=True), self.source_snapshot)
        self.assertEqual(self.sqlite_metadata(self.index), self.source_metadata)
        self.assertEqual(self.file_snapshot(self.index, self.source_snapshot), self.source_files)

    def test_missing_shared_base_and_stale_pointer_are_rejected(self):
        for fault in ("missing-binary", "missing-manifest", "stale-pointer"):
            with self.subTest(fault=fault):
                directory = self.trial / fault
                directory.mkdir()
                index = directory / "index.sqlite"
                runner.copy_index(self.index, index, self.workspace, self.commit, symbol_only=True)
                self.assertTrue(runner.native_ann_ready(index, self.symbol_metadata, symbol_only=True))
                if fault == "stale-pointer":
                    with closing(sqlite3.connect(index)) as db, db:
                        db.execute("UPDATE metadata SET value=? WHERE key='index_state_symbols'", ("0" * 64,))
                else:
                    pointer = runner.read_json(Path(str(index) + ".symbols.shared.json"))
                    binding = self.sqlite_metadata(index)["global_path"]
                    path = Path(binding + ".indexes") / pointer["contract"] / (pointer["base_id"] + ".usearch")
                    if fault == "missing-manifest":
                        path = Path(str(path) + ".manifest.json")
                    path.unlink()
                self.assertFalse(runner.native_ann_ready(index, self.symbol_metadata, symbol_only=True))
                with self.assertRaises((OSError, ValueError)):
                    runner.native_ann_snapshot(index, symbol_only=True)
                with self.assertRaises((OSError, ValueError)):
                    runner.verify_native_ann_reuse(self.binary, self.workspace, self.config_path,
                                                  index, directory, 30, symbol_only=True)
                self.assertFalse((directory / "ann-validation.json").exists())
        self.assertEqual(self.requests, self.symbol_requests)
        self.assertEqual(runner.native_ann_snapshot(self.index, symbol_only=True), self.source_snapshot)
        self.assertEqual(self.file_snapshot(self.index, self.source_snapshot), self.source_files)

    def test_real_map_preparation_needs_no_embeddings_or_ann(self):
        tasks = [{"targets": [{"path": "source.go", "symbol": name} for name in ("alpha", "beta", "gamma")]}]
        configuration = {"slopdex": dict(self.config), "index_timeout_seconds": 30}
        with patch.object(runner, "SUBMODULE", self.source):
            cache, metadata = runner.prepare_map_index(configuration, self.commit, self.trial / "map-cache", self.binary, tasks, symbols=False)
        index = cache / "index.sqlite"
        self.assertEqual(metadata["coverage"]["validated_targets"], 3)
        self.assertEqual(metadata["identity"]["kind"], "map")
        with runner.open_index(index) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM embeddings").fetchone()[0], 0)
        self.assertFalse(Path(str(index) + ".symbols.shared.json").exists())
        destination = self.trial / "map-copy.sqlite"
        runner.copy_index(index, destination, self.workspace, self.commit, include_ann=False)
        self.assertEqual(runner.index_coverage(destination, tasks, semantic=False)["validated_targets"], 3)
        self.assertFalse(Path(str(destination) + ".symbols.shared.json").exists())
        copied_config = self.trial / "map-config.json"
        runner.write_json(copied_config, runner.index_config(cache / "config.json", destination))
        output = runner.command([self.binary, "--root", str(self.workspace), "--config", str(copied_config),
                                 "--index", str(destination), "--no-reindex", "map", "--private", "-e", "alpha"], cwd=self.workspace)
        self.assertIn("alpha", output)
        self.assertEqual(self.requests, self.symbol_requests)

    def test_symbol_map_snapshot_reuses_vectors_and_graph_offline(self):
        index = self.symbol_cache / "index.sqlite"
        with runner.open_index(index) as db:
            self.assertGreater(db.execute("SELECT count(*) FROM embeddings").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM unit_embeddings").fetchone()[0], 0)
        snapshot = runner.native_ann_snapshot(index, symbol_only=True)
        self.assertEqual(set(snapshot["artifacts"]), {".symbols.shared.json"})
        destination = self.trial / "symbols.sqlite"
        runner.copy_index(index, destination, self.workspace, self.commit, symbol_only=True)
        result = runner.verify_native_ann_reuse(self.binary, self.workspace, self.symbol_cache / "config.json",
                                               destination, self.trial, 30, symbol_only=True)
        self.assertTrue(result["validated"])
        self.assertEqual(result["symbol_query"]["command"], "map -q")
        self.assertTrue(result["symbol_query"]["provider_credentials_removed"])
        self.assertTrue(result["symbol_query"]["selector_cache_cleared"])
        self.assertNotIn("external model call", (self.trial / "ann-validation" / "stderr.log").read_text())
        copied_config = self.trial / "symbol-config.json"
        runner.write_json(copied_config, runner.index_config(self.symbol_cache / "config.json", destination))
        output = runner.command([self.binary, "--root", str(self.workspace), "--config", str(copied_config),
                                 "--index", str(destination), "--no-reindex", "map", "--private", "-g", "*.go",
                                 "-q", "alpha", "--symbol-threshold", "0.99"], cwd=self.workspace)
        self.assertIn("func alpha", output)
        self.assertEqual(runner.native_ann_snapshot(destination, symbol_only=True), result["snapshot"])
        self.assertEqual(self.requests, self.symbol_requests)

    def test_symbol_preflight_rejects_stale_graph(self):
        destination = self.trial / "stale-symbols.sqlite"
        runner.copy_index(self.symbol_cache / "index.sqlite", destination, self.workspace, self.commit, symbol_only=True)
        pointer = runner.read_json(Path(str(destination) + ".symbols.shared.json"))
        binding = self.sqlite_metadata(destination)["global_path"]
        path = Path(binding + ".indexes") / pointer["contract"] / (pointer["base_id"] + ".base.json")
        manifest = runner.read_json(path)
        manifest["dimensions"] += 1
        runner.write_json(path, manifest)
        with self.assertRaisesRegex(ValueError, "stale or corrupt"):
            runner.verify_native_ann_reuse(self.binary, self.workspace, self.symbol_cache / "config.json",
                                          destination, self.trial, 30, symbol_only=True)
        self.assertFalse((self.trial / "ann-validation.json").exists())
        self.assertEqual(self.requests, self.symbol_requests)

    def test_real_cli_help_clusters_and_protected_overrides_are_not_navigation(self):
        sandbox = self.trial / "map"
        sandbox.mkdir()
        binaries, log = runner.install_wrapper(sandbox, self.workspace, "map", self.binary, self.symbol_cache, self.commit)
        wrapper = str(binaries / "slopdex")
        result = runner.command([wrapper, "map", "-ih"], cwd=self.workspace)
        self.assertIn("Usage:", result)
        calls = runner.read_slopdex_calls(log)
        self.assertEqual(runner.slopdex_command_counts(calls), {"help": 1})
        self.assertEqual(runner.slopdex_protocol_violations("map", calls), [
            {"reason": "required_command_not_used_successfully", "command": "map"}])
        other_index = sandbox / "other.sqlite"
        with self.assertRaisesRegex(RuntimeError, "unavailable"):
            runner.command([wrapper, "map", "--index", str(other_index)], cwd=self.workspace)
        self.assertFalse(other_index.exists())
        with self.assertRaisesRegex(RuntimeError, "unavailable"):
            runner.command([wrapper, "help", "models"], cwd=self.workspace)
        self.assertEqual(self.requests, self.symbol_requests)


if __name__ == "__main__":
    unittest.main()
