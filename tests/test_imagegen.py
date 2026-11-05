import base64
import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
import zlib
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

    def image(self, *extra, prompt=None, **kwargs):
        return self.ok("image", "-o", self.output, "-f", "-", "-b", self.base, *extra,
                       input=prompt or self.prompt, **kwargs)

    def test_one_call_renders_the_output_with_the_default_model(self):
        handler = self.server()
        result = self.image("-s", "2560x1440", "--keep-prompt", umask="022")
        request = handler.requests[0]
        self.assertEqual(request["path"], "/v1/images/generations")
        self.assertEqual({k: request["data"][k] for k in ("model", "quality", "size", "output_format", "prompt")},
                         {"model": "gpt-image-2.6-sunburst", "quality": "medium", "size": "2560x1440",
                          "output_format": "webp",
                          "prompt": self.prompt + "\n\nLandscape orientation, 16:9 aspect ratio, wider than tall."})
        self.assertEqual(result["file"], str(self.output))
        self.assertEqual(self.output.read_bytes(), b"image-bytes")
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o644)
        self.assertEqual((self.work / "docs/arch.prompt.txt").read_text(), self.prompt)
        self.assertEqual(result["budget"], {"images": "1/3", "edits": "0/1"})
        self.assertIn("costs as much", result["next"])
        self.assertNotIn("previous", result)

    def test_model_and_quality_can_be_chosen(self):
        handler = self.server()
        self.image("-m", "gpt-image-2.6-flare", "-q", "high")
        self.assertEqual((handler.requests[0]["data"]["model"], handler.requests[0]["data"]["quality"]),
                         ("gpt-image-2.6-flare", "high"))

    def test_a_revised_prompt_starts_over_and_keeps_the_previous_render(self):
        handler = self.server()
        self.image("-s", "1024x1536")
        handler.response_data = {"data": [{"b64_json": base64.b64encode(b"second").decode()}]}
        second = self.image(prompt="Revised layout.")
        self.assertEqual(handler.requests[1]["data"]["size"], "1024x1536")
        self.assertEqual(self.output.read_bytes(), b"second")
        self.assertEqual(Path(second["previous"]).read_bytes(), b"image-bytes")
        self.assertEqual(second["image"], 2)

    def test_reports_and_converts_what_the_gateway_really_returned(self):
        png = b"\x89PNG\r\n\x1a\n" + b"\x00\x00\x00\rIHDR" + (1672).to_bytes(4, "big") + (941).to_bytes(4, "big")
        handler = self.server(data={"data": [{"b64_json": base64.b64encode(png).decode()}], "quality": "low"})
        bin_dir = self.work / "bin"
        bin_dir.mkdir()
        (bin_dir / "python3").symlink_to(shutil.which("python3"))
        result = self.ok("image", "-o", self.output, "-p", "x", "-s", "2560x1440", "-b", self.base,
                         env=self.env(PATH=str(bin_dir)))
        self.assertEqual(result["returned"], "png 1672x941")
        self.assertIn("requested webp 2560x1440", result["warning"])
        self.assertIn("rendered low", result["warning"])
        self.assertEqual(result["quality"], "low")
        self.assertEqual(self.output.read_bytes(), png)  # no ffmpeg: written as sent
        self.assertIn("no ffmpeg", result["warning"])
        self.ok("edit", "-o", self.output, "-p", "Make the title bigger.", "-b", self.base,
                env=self.env(PATH=str(bin_dir)))
        self.assertIn(b"Content-Type: image/png", handler.requests[1]["data"]["_raw"])

    def test_another_format_is_converted_with_ffmpeg(self):
        if not shutil.which("ffmpeg"):
            self.skipTest("ffmpeg not installed")
        self.server(data={"data": [{"b64_json": base64.b64encode(self.ref("x").read_bytes()).decode()}]})
        result = self.image()
        self.assertEqual(result["returned"], "png 16x16")
        self.assertEqual(self.output.read_bytes()[8:12], b"WEBP")
        self.assertIn("converted to webp", result["warning"])

    def test_budgets_cap_images_and_edits(self):
        handler = self.server()
        early = self.run_cmd("edit", "-o", self.output, "-p", "fix", "-b", self.base)
        self.assertEqual(early.returncode, 3)
        for number in range(3):
            self.image(prompt=f"Layout {number}.")
        over = self.run_cmd("image", "-o", self.output, "-p", "x", "-b", self.base)
        self.assertEqual(over.returncode, 3)
        self.assertIn("--allow-extra", json.loads(over.stdout)["reason"])

        self.output.write_bytes(b"chosen-bytes")
        edit = self.ok("edit", "-o", self.output, "-p", 'Fix "Anthropic".', "-b", self.base)
        self.assertEqual(Path(edit["previous"]).read_bytes(), b"chosen-bytes")
        self.assertIn(b"chosen-bytes", handler.requests[-1]["data"]["_raw"])
        self.assertTrue(handler.requests[-1]["data"]["prompt"].startswith('Fix "Anthropic".\n\n'))
        self.assertEqual(handler.requests[-1]["data"]["model"], "gpt-image-2.6-sunburst")
        second = self.run_cmd("edit", "-o", self.output, "-p", "again", "-b", self.base)
        self.assertEqual(second.returncode, 3)
        self.assertEqual(len(handler.requests), 4)
        self.ok("edit", "-o", self.output, "-p", "again", "--allow-extra", "-b", self.base)

        status = self.ok("status", "-o", self.output)
        self.assertEqual(status["budget"], {"images": "3/3", "edits": "2/1"})
        self.ok("reset", "-o", self.output)
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["images"], "0/3")

    def test_an_edit_must_say_what_to_change(self):
        handler = self.server()
        self.image()
        unchanged = self.run_cmd("edit", "-o", self.output, "-f", "-", "-b", self.base, input=self.prompt)
        self.assertEqual(unchanged.returncode, 3)
        self.assertIn("must say what to change", json.loads(unchanged.stdout)["reason"])
        self.assertEqual(len(handler.requests), 1)

    def test_state_from_the_draft_workflow_starts_fresh(self):
        self.server()
        self.image()
        state = next((self.work / "state/openai-image-gen").glob("*/state.json"))
        state.write_text(json.dumps({"output": str(self.output), "size": None, "drafts": [], "finals": 1, "edits": 0}))
        self.assertEqual(self.ok("status", "-o", self.output)["budget"], {"images": "0/3", "edits": "0/1"})

    def test_prompt_states_the_shape_because_gateways_ignore_size(self):
        handler = self.server()
        for size, line in [("1024x1024", "Square 1:1 composition."),
                           ("1024x1536", "Portrait orientation, 2:3 aspect ratio, taller than wide."),
                           ("1600x1008", "Landscape orientation, 1.59:1 aspect ratio, wider than tall.")]:
            with self.subTest(size=size):
                self.ok("image", "-o", self.work / f"{size}.webp", "-p", "Cat.", "-s", size, "-b", self.base)
                self.assertTrue(handler.requests[-1]["data"]["prompt"].endswith("\n\n" + line))

    def test_the_same_image_is_not_paid_for_twice(self):
        handler = self.server()
        self.image()
        again = self.run_cmd("image", "-o", self.output, "-f", "-", "-b", self.base, input=" " + self.prompt + "\n")
        self.assertEqual(again.returncode, 3)
        self.assertIn("image 1 already used this prompt", json.loads(again.stdout)["reason"])
        self.assertEqual(len(handler.requests), 1)
        self.image("-s", "1024x1536")
        self.assertEqual(len(handler.requests), 2)

    def ref(self, name, color=b"\xff\x80\x00"):
        row = b"\x00" + color * 16
        chunk = lambda kind, data: (len(data).to_bytes(4, "big") + kind + data
                                    + zlib.crc32(kind + data).to_bytes(4, "big"))
        path = self.work / "cast" / name / "sheet.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", (16).to_bytes(4, "big") * 2 + bytes([8, 2, 0, 0, 0]))
                         + chunk(b"IDAT", zlib.compress(row * 16)) + chunk(b"IEND", b""))
        return path

    def images_sent(self, request):
        return re.findall(rb'name="image\[\]"; filename="([^"]+)"', request["data"]["_raw"])

    def test_reference_images_carry_over_to_retries_and_edits(self):
        handler = self.server()
        he, she = self.ref("he"), self.ref("she", b"\x00\x40\xff")
        first = self.image("-s", "2560x1440", "--ref", he, "-r", she)
        request = handler.requests[0]
        self.assertEqual(request["path"], "/v1/images/edits")
        self.assertEqual(self.images_sent(request), [b"sheet.png", b"sheet.png"])
        self.assertIn(he.read_bytes(), request["data"]["_raw"])
        self.assertIn("image 1 (he/sheet.png), image 2 (she/sheet.png)", request["data"]["prompt"])
        self.assertTrue(request["data"]["prompt"].rstrip().endswith("wider than tall."))
        self.assertEqual(first["references"], [str(he), str(she)])

        retry = self.image(prompt="Same scene, seen from the left.")
        self.assertEqual(self.images_sent(handler.requests[1]), [b"sheet.png", b"sheet.png"])
        self.assertEqual(retry["references"], [str(he), str(she)])

        he.write_bytes(self.ref("other").read_bytes() + b"\x00")
        edited = self.ok("edit", "-o", self.output, "-p", "Only change: add a red scarf.", "-b", self.base)
        self.assertEqual(self.images_sent(handler.requests[2]), [b"before-edit-1.webp", b"sheet.png", b"sheet.png"])
        self.assertIn("image 2 (he/sheet.png), image 3 (she/sheet.png)", handler.requests[2]["data"]["prompt"])
        self.assertIn("changed since", edited["warning"])

        self.image("--no-refs", prompt="No characters this time.")
        self.assertEqual(handler.requests[3]["path"], "/v1/images/generations")

    def test_references_change_what_counts_as_the_same_image(self):
        handler = self.server()
        self.image()
        self.image("--ref", self.ref("he"))
        self.assertEqual(len(handler.requests), 2)
        again = self.run_cmd("image", "-o", self.output, "-f", "-", "-r", self.ref("he"), "-b", self.base,
                             input=self.prompt)
        self.assertEqual(again.returncode, 3)
        self.assertIn("prompt, size and references", json.loads(again.stdout)["reason"])

    def test_a_gone_reference_is_refused_until_passed_again(self):
        handler = self.server()
        he = self.ref("he")
        self.image("--ref", he)
        he.unlink()
        gone = self.run_cmd("image", "-o", self.output, "-p", "Retry.", "-b", self.base)
        self.assertEqual(gone.returncode, 3)
        self.assertIn("pass --ref again", json.loads(gone.stdout)["reason"])
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["images"], "1/3")
        self.ok("image", "-o", self.output, "-p", "Retry.", "--ref", self.ref("she"), "-b", self.base)
        self.assertEqual(self.images_sent(handler.requests[-1]), [b"sheet.png"])
        self.assertIn("image 1 (she/sheet.png)", handler.requests[-1]["data"]["prompt"])

    def test_reference_validation(self):
        self.server()
        not_image = self.work / "notes.txt"
        not_image.write_text("hello")
        for refs, message in [(["missing.png"], "not found"), ([not_image], "png, webp or jpeg"),
                              ([self.ref("he")] * 9, "at most 8")]:
            with self.subTest(message=message):
                args = [arg for ref in refs for arg in ("--ref", ref)]
                result = self.run_cmd("image", "-o", self.output, "-p", "x", *args, "-b", self.base)
                self.assertEqual(result.returncode, 1)
                self.assertIn(message, result.stderr)
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["images"], "0/3")

    def test_budgets_are_per_output_path(self):
        self.server()
        self.image()
        other = self.ok("status", "-o", self.work / "other.webp")
        self.assertEqual(other["budget"]["images"], "0/3")

    def test_failed_calls_spend_nothing_and_keep_the_output(self):
        self.output.parent.mkdir(parents=True, exist_ok=True)
        self.output.write_bytes(b"original")
        for status, data in [(500, {"error": {"message": "rejected"}}),
                             (200, {"data": [{"b64_json": "invalid!"}]}),
                             (200, {"data": [{"url": "http://example.com/image.png"}]})]:
            with self.subTest(status=status, data=data):
                self.server(status, data)
                result = self.run_cmd("image", "-o", self.output, "-p", "x", "-b", self.base)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(self.output.read_bytes(), b"original")
                self.assertEqual(self.ok("status", "-o", self.output)["budget"]["images"], "0/3")
        self.server(400, {"error": {"code": "moderation_blocked", "message": "Your request was rejected by the safety system."}})
        refused = self.run_cmd("image", "-o", self.output, "-p", "x", "-b", self.base)
        self.assertIn("(HTTP 400): moderation_blocked: Your request was rejected", refused.stderr)
        self.assertEqual(self.ok("status", "-o", self.output)["budget"]["images"], "0/3")

    def test_prompt_and_option_validation(self):
        blank = self.work / "blank.txt"
        blank.write_text("  \n")
        cases = [(("image", "-o", "x.webp", "-p", "a", "-f", "-"), 1),
                 (("image", "-o", "x.webp", "-f", blank), 1),
                 (("image", "-o", "x.webp", "-f", self.work / "none.txt"), 1),
                 (("image", "-o", "x.webp", "-p", "a", "-s", "1000x1000"), 1),
                 (("image", "-o", "x.webp", "-p", "a", "-s", "3200x800"), 1),
                 (("image", "-o", "x.png", "-p", "a", "--compression", "80"), 2),
                 (("image", "-o", "x.png", "-p", "a", "--format", "gif"), 2),
                 (("image", "-o", "x.webp", "-p", "a", "-q", "ultra"), 2),
                 (("draft", "-o", "x.webp", "-p", "a"), 2),
                 (("image", "--prompt", "a"), 2)]
        for args, code in cases:
            with self.subTest(args=args):
                self.assertEqual(self.run_cmd(*args).returncode, code)

    def test_format_and_compression(self):
        handler = self.server()
        self.output = self.work / "poster.jpg"
        self.image("--compression", "80")
        self.assertEqual(handler.requests[0]["data"]["output_format"], "jpeg")
        self.assertEqual(handler.requests[0]["data"]["output_compression"], 80)

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
        self.ok("image", "-o", self.output, "-p", "x", env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer provider-key")
        self.assertEqual(handler.requests[0]["path"], "/v1/images/generations")

    def test_provider_key_never_goes_to_another_url(self):
        handler = self.server()
        env = self.codex_home('base_url = "http://invalid.example/v1"\n'
                              'http_headers = { "Authorization" = "Bearer provider-key" }\n', auth_key="auth-key")
        self.ok("image", "-o", self.output, "-p", "x", "-b", self.base, env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer auth-key")

    def test_auth_json_follows_provider_that_requires_openai_auth(self):
        handler = self.server()
        env = self.codex_home(f'base_url = "{self.base}"\nrequires_openai_auth = true\n', auth_key="auth-key")
        self.ok("image", "-o", self.output, "-p", "x", env=env)
        self.assertEqual(handler.requests[0]["auth"], "Bearer auth-key")

    def test_missing_key_fails_before_any_request(self):
        handler = self.server()
        env = self.env()
        del env["OPENAI_API_KEY"]
        result = self.run_cmd("image", "-o", self.output, "-p", "x", "-b", self.base, env=env)
        self.assertEqual(result.returncode, 1)
        self.assertIn("API key not found", result.stderr)
        self.assertEqual(handler.requests, [])


if __name__ == "__main__":
    unittest.main()
