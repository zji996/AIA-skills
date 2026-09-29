import base64
import json
import os
import re
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
IMAGEGEN = ROOT / "skills/openai-image-gen/scripts/imagegen.py"


class ImageHandler(BaseHTTPRequestHandler):
    response_status = 200
    response_data = {"data": [{"b64_json": base64.b64encode(b"image-bytes").decode()}]}
    requests = None

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.headers.get("Content-Type", "").startswith("multipart/"):
            fields = dict(re.findall(rb'name="([^"]+)"\r\n\r\n(.*?)\r\n--', body, re.S))
            data = {key.decode(): value.decode() for key, value in fields.items()}
            data["_raw"] = body
        else:
            data = json.loads(body)
        type(self).requests.append({"path": self.path, "auth": self.headers.get("Authorization"), "data": data})
        payload = json.dumps(self.response_data).encode()
        self.send_response(self.response_status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


class ImagegenTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="aia-imagegen-")
        self.addCleanup(temp.cleanup)
        self.work = Path(temp.name)
        self.output = self.work / "docs/arch.webp"
        self.prompt = 'Diagram.\nTEXT RULES: only "长期记忆" and "会话 1"; quotes stay intact.'

    def server(self, status=200, data=None):
        handler = type("Response", (ImageHandler,), {
            "response_status": status,
            "response_data": data if data is not None else ImageHandler.response_data,
            "requests": [],
        })
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.base = f"http://127.0.0.1:{server.server_port}/v1"
        return handler

    def env(self, **extra):
        env = {key: value for key, value in os.environ.items() if key not in ("OPENAI_API_KEY", "OPENAI_BASE_URL")}
        return {**env, "OPENAI_API_KEY": "test-only", "CODEX_HOME": str(self.work / "no-codex"),
                "XDG_STATE_HOME": str(self.work / "state"), **extra}

    def run_cmd(self, *args, env=None, input=None, umask=None):
        command = [str(IMAGEGEN), *map(str, args)]
        if umask is not None:
            command = ["bash", "-c", f'umask {umask} && exec "$@"', "_", *command]
        return subprocess.run(command, cwd=self.work, env=env or self.env(), input=input,
                              capture_output=True, text=True, timeout=15)

    def ok(self, *args, **kwargs):
        result = self.run_cmd(*args, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def draft(self, *extra):
        return self.ok("draft", "-o", self.output, "-f", "-", "-b", self.base, *extra, input=self.prompt)

    def test_draft_is_cheap_scaled_webp_outside_the_project(self):
        handler = self.server()
        result = self.draft("-s", "2560x1440")
        request = handler.requests[0]
        self.assertEqual(request["path"], "/v1/images/generations")
        self.assertEqual({k: request["data"][k] for k in ("model", "quality", "size", "output_format", "prompt")},
                         {"model": "gpt-image-2.5-flare", "quality": "low", "size": "1536x864",
                          "output_format": "webp", "prompt": self.prompt})
        self.assertTrue(result["file"].startswith(str(self.work / "state/openai-image-gen")))
        self.assertEqual(Path(result["file"]).read_bytes(), b"image-bytes")
        self.assertFalse(self.output.exists())
        self.assertEqual(result["budget"]["drafts"], "1/3")
        self.assertIn("final", result["next"])

    def test_final_renders_once_from_the_draft_as_layout_reference(self):
        handler = self.server()
        self.draft("-s", "2560x1440")
        result = self.ok("final", "-o", self.output, "--keep-prompt", "-b", self.base, umask="022")
        request = handler.requests[1]
        self.assertEqual(request["path"], "/v1/images/edits")
        self.assertEqual(request["data"]["model"], "gpt-image-2.5-sunburst")
        self.assertEqual(request["data"]["quality"], "high")
        self.assertEqual(request["data"]["size"], "2560x1440")
        self.assertEqual(request["data"]["output_format"], "webp")
        self.assertTrue(request["data"]["prompt"].startswith(self.prompt))
        self.assertIn("approved draft", request["data"]["prompt"])
        self.assertIn(b"image-bytes", request["data"]["_raw"])
        self.assertEqual(self.output.read_bytes(), b"image-bytes")
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o644)
        self.assertEqual((self.work / "docs/arch.prompt.txt").read_text(), self.prompt)
        self.assertEqual(result["budget"]["finals"], "1/1")

        again = self.run_cmd("final", "-o", self.output, "-b", self.base)
        self.assertEqual(again.returncode, 3)
        self.assertEqual(json.loads(again.stdout)["status"], "refused")
        self.assertEqual(len(handler.requests), 2)
        self.ok("final", "-o", self.output, "--allow-extra", "--fresh", "-b", self.base)
        self.assertEqual(handler.requests[2]["path"], "/v1/images/generations")
        self.assertEqual(handler.requests[2]["data"]["prompt"], self.prompt)

    def test_reports_what_the_gateway_really_returned(self):
        png = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\rIHDR" + (1672).to_bytes(4, "big") + (941).to_bytes(4, "big")
        handler = self.server(data={"data": [{"b64_json": base64.b64encode(png).decode()}]})
        draft = self.draft("-s", "2560x1440")
        self.assertEqual(draft["returned"], "png 1672x941")
        self.assertIn("requested webp 1536x864", draft["warning"])
        self.assertTrue(draft["file"].endswith("draft-1.png"))
        final = self.ok("final", "-o", self.output, "-b", self.base)
        self.assertIn("requested webp 2560x1440", final["warning"])
        self.assertIn(b"Content-Type: image/png", handler.requests[1]["data"]["_raw"])

    def test_final_needs_a_draft_and_takes_a_corrected_prompt(self):
        handler = self.server()
        refused = self.run_cmd("final", "-o", self.output, "-b", self.base)
        self.assertEqual(refused.returncode, 3)
        self.assertEqual(handler.requests, [])
        self.draft()
        self.draft()
        self.ok("final", "-o", self.output, "--draft", "1", "-p", "Corrected label.", "-b", self.base)
        self.assertTrue(handler.requests[2]["data"]["prompt"].startswith("Corrected label."))
        self.assertEqual(handler.requests[2]["data"]["size"], "1536x1024")

    def test_budgets_cap_drafts_and_edits(self):
        handler = self.server()
        for _ in range(3):
            self.draft()
        over = self.run_cmd("draft", "-o", self.output, "-p", "x", "-b", self.base)
        self.assertEqual(over.returncode, 3)
        self.assertIn("--allow-extra", json.loads(over.stdout)["reason"])

        early = self.run_cmd("edit", "-o", self.output, "-p", "fix", "-b", self.base)
        self.assertEqual(early.returncode, 3)
        self.ok("final", "-o", self.output, "-b", self.base)
        self.output.write_bytes(b"final-bytes")
        edit = self.ok("edit", "-o", self.output, "-p", 'Fix "Anthropic".', "-b", self.base)
        self.assertEqual(Path(edit["previous"]).read_bytes(), b"final-bytes")
        self.assertIn(b"final-bytes", handler.requests[-1]["data"]["_raw"])
        self.assertEqual(handler.requests[-1]["data"]["prompt"], 'Fix "Anthropic".')
        second = self.run_cmd("edit", "-o", self.output, "-p", "again", "-b", self.base)
        self.assertEqual(second.returncode, 3)
        self.assertEqual(len(handler.requests), 5)

        status = self.ok("status", "-o", self.output)
        self.assertEqual(status["budget"], {"drafts": "3/3", "finals": "1/1", "edits": "1/1"})
        self.ok("reset", "-o", self.output)
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["drafts"], "0/3")

    def test_budgets_are_per_output_path(self):
        self.server()
        self.draft()
        other = self.ok("status", "-o", self.work / "other.webp")
        self.assertEqual(other["budget"]["drafts"], "0/3")

    def test_failed_calls_spend_nothing_and_keep_the_output(self):
        for status, data in [(500, {"error": {"message": "rejected"}}),
                             (200, {"data": [{"b64_json": "invalid!"}]}),
                             (200, {"data": [{"url": "http://example.com/image.png"}]})]:
            with self.subTest(status=status, data=data):
                self.server(status, data)
                result = self.run_cmd("draft", "-o", self.output, "-p", "x", "-b", self.base)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(self.ok("status", "-o", self.output)["budget"]["drafts"], "0/3")
        self.server()
        self.draft()
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.output.write_bytes(b"original")
        self.server(500, {"error": {"message": "rejected"}})
        failed = self.run_cmd("final", "-o", self.output, "-b", self.base)
        self.assertIn("rejected", failed.stderr)
        self.assertEqual(self.output.read_bytes(), b"original")
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["finals"], "0/1")

    def test_prompt_and_option_validation(self):
        blank = self.work / "blank.txt"
        blank.write_text("  \n")
        cases = [(("draft", "-o", "x.webp", "-p", "a", "-f", "-"), 1),
                 (("draft", "-o", "x.webp", "-f", blank), 1),
                 (("draft", "-o", "x.webp", "-f", self.work / "none.txt"), 1),
                 (("draft", "-o", "x.webp", "-p", "a", "-s", "1000x1000"), 1),
                 (("draft", "-o", "x.webp", "-p", "a", "-s", "3200x800"), 1),
                 (("final", "-o", "x.png", "--compression", "80"), 2),
                 (("final", "-o", "x.png", "--format", "gif"), 2),
                 (("draft", "-o", "x.webp", "-p", "a", "-q", "ultra"), 2),
                 (("draft", "--prompt", "a"), 2)]
        for args, code in cases:
            with self.subTest(args=args):
                self.assertEqual(self.run_cmd(*args).returncode, code)

    def test_final_format_and_compression(self):
        handler = self.server()
        self.output = self.work / "poster.jpg"
        self.draft()
        self.ok("final", "-o", self.output, "--compression", "80", "-b", self.base)
        self.assertEqual(handler.requests[1]["data"]["output_format"], "jpeg")
        self.assertEqual(handler.requests[1]["data"]["output_compression"], "80")

    def codex_home(self, provider_lines, auth_key=None):
        config = self.work / "codex"
        config.mkdir(exist_ok=True)
        (config / "config.toml").write_text('model_provider = "selected"\n'
                                            '[model_providers.decoy]\nbase_url = "http://invalid.example/v1"\n'
                                            '[model_providers.selected]\n' + provider_lines)
        if auth_key:
            (config / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": auth_key}))
        env = self.env(CODEX_HOME=str(config))
        del env["OPENAI_API_KEY"]
        return env

    def test_selected_codex_provider_wins_over_auth_json(self):
        handler = self.server()
        env = self.codex_home(f'base_url = "{self.base}"\n'
                              'http_headers = { "Authorization" = "Bearer provider-key" }\n', auth_key="auth-key")
        self.ok("draft", "-o", self.output, "-p", "x", env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer provider-key")
        self.assertEqual(handler.requests[0]["path"], "/v1/images/generations")

    def test_provider_key_never_goes_to_another_url(self):
        handler = self.server()
        env = self.codex_home('base_url = "http://invalid.example/v1"\n'
                              'http_headers = { "Authorization" = "Bearer provider-key" }\n', auth_key="auth-key")
        self.ok("draft", "-o", self.output, "-p", "x", "-b", self.base, env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer auth-key")

    def test_auth_json_follows_provider_that_requires_openai_auth(self):
        handler = self.server()
        env = self.codex_home(f'base_url = "{self.base}"\nrequires_openai_auth = true\n', auth_key="auth-key")
        self.ok("draft", "-o", self.output, "-p", "x", env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer auth-key")

    def test_missing_key_fails_before_any_request(self):
        handler = self.server()
        env = self.env()
        del env["OPENAI_API_KEY"]
        result = self.run_cmd("draft", "-o", self.output, "-p", "x", "-b", self.base, env=env)
        self.assertEqual(result.returncode, 1)
        self.assertIn("API key not found", result.stderr)
        self.assertEqual(handler.requests, [])


if __name__ == "__main__":
    unittest.main()
