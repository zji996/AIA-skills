#!/usr/bin/env python3
"""Draft cheaply, then render one final image; budgets are enforced per output file.

Every command prints one JSON line. State (drafts, counts, prompt) lives outside the
project in $XDG_STATE_HOME/openai-image-gen/<hash of the output path>/.
"""
import argparse
import base64
import hashlib
import json
import os
import sys
import time
import tomllib
import urllib.error
import urllib.request
import uuid
from pathlib import Path

DRAFT_MODEL, DRAFT_QUALITY, DRAFT_LONG_EDGE = "gpt-image-2.5-flare", "low", 1536
FINAL_MODEL, FINAL_QUALITY = "gpt-image-2.5-sunburst", "high"
BUDGET = {"drafts": 3, "finals": 1, "edits": 1}
LAYOUT_LOCK = ("\n\nThe attached image is the approved draft. Use it only as the layout reference: keep its "
               "composition, element positions and arrows. Render everything at full quality, with crisp, "
               "exactly spelled text as listed above.")


class Refusal(Exception):
    """A request the budget or workflow does not allow; exit code 3."""


def emit(**fields):
    print(json.dumps(fields, ensure_ascii=False))


def state_dir(output):
    root = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "openai-image-gen"
    return root / hashlib.sha256(str(Path(output).resolve()).encode()).hexdigest()[:16]


def load_state(output):
    path = state_dir(output) / "state.json"
    if path.is_file():
        return json.loads(path.read_text())
    return {"output": str(Path(output).resolve()), "size": None, "drafts": [], "finals": 0, "edits": 0}


def save_state(output, state):
    directory = state_dir(output)
    directory.mkdir(parents=True, exist_ok=True)
    tmp = directory / "state.json.tmp"
    tmp.write_text(json.dumps(state, ensure_ascii=False, indent=2))
    os.replace(tmp, directory / "state.json")


def budget_view(state):
    return {"drafts": f"{len(state['drafts'])}/{BUDGET['drafts']}",
            "finals": f"{state['finals']}/{BUDGET['finals']}",
            "edits": f"{state['edits']}/{BUDGET['edits']}"}


def spend(state, kind, allow_extra):
    used = len(state["drafts"]) if kind == "drafts" else state[kind]
    if used >= BUDGET[kind] and not allow_extra:
        raise Refusal(f"{kind} budget used up ({used}/{BUDGET[kind]}); "
                      "go on only with the user's explicit approval, via --allow-extra")


def read_prompt(args):
    if args.prompt and args.prompt_file:
        raise SystemExit("error: use either --prompt or --prompt-file")
    text = args.prompt or ""
    if args.prompt_file:
        source = sys.stdin if args.prompt_file == "-" else None
        if source is None and not Path(args.prompt_file).is_file():
            raise SystemExit(f"error: prompt file not found: {args.prompt_file}")
        text = source.read() if source else Path(args.prompt_file).read_text(encoding="utf-8")
    return text if text.strip() else None


def parse_size(value):
    try:
        width, height = (int(part) for part in value.lower().split("x"))
    except ValueError:
        raise SystemExit(f"error: size must look like 1536x1024, got {value}")
    if width % 16 or height % 16 or max(width, height) > 3840 or max(width, height) > 3 * min(width, height):
        raise SystemExit("error: size edges must be multiples of 16, at most 3840, ratio at most 3:1")
    return width, height


def checked_size(value):
    return value and "x".join(map(str, parse_size(value)))


def draft_size(width, height):
    scale = min(1.0, DRAFT_LONG_EDGE / max(width, height))
    return f"{max(16, round(width * scale / 16) * 16)}x{max(16, round(height * scale / 16) * 16)}"


def image_format(output, explicit):
    fmt = explicit or {".webp": "webp", ".jpg": "jpeg", ".jpeg": "jpeg"}.get(Path(output).suffix.lower(), "png")
    if fmt not in ("png", "webp", "jpeg"):
        raise SystemExit("error: --format must be png, webp or jpeg")
    return fmt


# --- credentials: CLI -> env -> selected Codex provider -> Codex auth.json -----------------

def credentials(args):
    base_url = args.base_url or os.environ.get("OPENAI_BASE_URL", "")
    api_key = args.api_key or os.environ.get("OPENAI_API_KEY", "")
    codex = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex")
    provider_url = provider_key = ""
    openai_auth = False
    if (codex / "config.toml").is_file():
        try:
            config = tomllib.loads((codex / "config.toml").read_text())
            provider = config.get("model_providers", {}).get(config.get("model_provider", "openai"), {})
            headers = {k.lower(): v for k, v in provider.get("http_headers", {}).items()}
            bearer = headers.get("authorization", "")
            provider_url = provider.get("base_url", "")
            provider_key = bearer.split(" ", 1)[1] if bearer.lower().startswith("bearer ") else ""
            openai_auth = bool(provider.get("requires_openai_auth", False))
        except (OSError, ValueError, TypeError, AttributeError) as exc:
            raise SystemExit(f"error: cannot read selected Codex provider: {exc}")
    # The selected provider's own key only ever goes to its own URL.
    if not api_key and provider_url and provider_key and base_url.rstrip("/") in ("", provider_url.rstrip("/")):
        api_key, base_url = provider_key, base_url or provider_url
    if not api_key and (codex / "auth.json").is_file():
        try:
            auth = json.loads((codex / "auth.json").read_text())
        except (OSError, ValueError):
            auth = {}
        api_key = auth.get("OPENAI_API_KEY") or auth.get("openai_api_key") or ""
        if api_key and not base_url and openai_auth:
            base_url = provider_url
    if not api_key:
        raise SystemExit("error: OpenAI API key not found; set OPENAI_API_KEY or configure ~/.codex")
    base_url = (base_url or "https://api.openai.com").rstrip("/")
    return base_url.removesuffix("/v1"), api_key


# --- HTTP -------------------------------------------------------------------------------

def multipart(fields, files):
    boundary = uuid.uuid4().hex
    parts = []
    for name, value in fields.items():
        parts.append(f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode())
    for name, path in files:
        head = (f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{Path(path).name}"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n")
        parts.append(head.encode() + Path(path).read_bytes() + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def request_image(args, *, model, quality, size, prompt, fmt, reference=None):
    base_url, api_key = credentials(args)
    fields = {"model": model, "prompt": prompt, "size": size, "quality": quality, "output_format": fmt, "n": 1}
    if getattr(args, "compression", None) is not None:
        fields["output_compression"] = args.compression
    if reference:
        body, content_type = multipart(fields, [("image[]", reference)])
        url = f"{base_url}/v1/images/edits"
    else:
        body, content_type = json.dumps(fields).encode(), "application/json"
        url = f"{base_url}/v1/images/generations"
    req = urllib.request.Request(url, data=body, method="POST",
                                 headers={"Authorization": f"Bearer {api_key}", "Content-Type": content_type})
    try:
        with urllib.request.urlopen(req, timeout=600) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as exc:
        try:
            detail = json.load(exc)
            message = (detail.get("error") or {}).get("message") or detail.get("message")
        except ValueError:
            message = None
        raise SystemExit(f"error: image request failed (HTTP {exc.code}): {message or exc.reason}")
    except (urllib.error.URLError, TimeoutError, ValueError) as exc:
        raise SystemExit(f"error: image request failed: {exc}")
    item = (payload.get("data") or [{}])[0]
    if item.get("b64_json"):
        try:
            return base64.b64decode(item["b64_json"], validate=True)
        except ValueError:
            raise SystemExit("error: invalid base64 image data")
    if str(item.get("url", "")).startswith("https://"):
        try:
            with urllib.request.urlopen(item["url"], timeout=120) as response:
                return response.read()
        except (urllib.error.URLError, TimeoutError) as exc:
            raise SystemExit(f"error: image download failed: {exc}")
    raise SystemExit("error: no image data in the API response")


def write_atomic(path, data):
    if not data:
        raise SystemExit("error: image response was empty")
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{uuid.uuid4().hex[:8]}")
    tmp.write_bytes(data)  # created under the caller's umask, unlike mkstemp's 0600
    os.replace(tmp, path)


# --- commands ---------------------------------------------------------------------------

def cmd_draft(args, state):
    prompt = read_prompt(args)
    if not prompt:
        raise SystemExit("error: draft needs a non-empty --prompt or --prompt-file")
    spend(state, "drafts", args.allow_extra)
    width, height = parse_size(args.size or state["size"] or "1536x1024")
    state["size"] = f"{width}x{height}"
    number = len(state["drafts"]) + 1
    directory = state_dir(args.output)
    started = time.monotonic()
    data = request_image(args, model=args.model or DRAFT_MODEL, quality=args.quality or DRAFT_QUALITY,
                         size=draft_size(width, height), prompt=prompt, fmt="webp")
    file = directory / f"draft-{number}.webp"
    write_atomic(file, data)
    (directory / f"draft-{number}.prompt.txt").write_text(prompt, encoding="utf-8")
    state["drafts"].append({"n": number, "file": str(file)})
    save_state(args.output, state)
    emit(status="ok", step="draft", draft=number, file=str(file), seconds=round(time.monotonic() - started),
         budget=budget_view(state),
         next=f"Review the draft for layout, arrows and text placement. If it works: final -o {args.output}"
              f" [--draft {number}]; otherwise fix the prompt and draft again.")


def cmd_final(args, state):
    if not state["drafts"]:
        raise Refusal("no draft yet; run draft first and review it")
    pick = args.draft or state["drafts"][-1]["n"]
    chosen = next((d for d in state["drafts"] if d["n"] == pick), None)
    if chosen is None:
        raise Refusal(f"draft {pick} does not exist")
    # A corrected prompt may replace the draft's, e.g. to fix a label the draft got wrong.
    prompt = read_prompt(args) or Path(chosen["file"]).with_name(f"draft-{pick}.prompt.txt").read_text(encoding="utf-8")
    spend(state, "finals", args.allow_extra)
    fmt = image_format(args.output, args.format)
    started = time.monotonic()
    data = request_image(args, model=args.model or FINAL_MODEL, quality=args.quality or FINAL_QUALITY,
                         size=checked_size(args.size) or state["size"], fmt=fmt,
                         prompt=prompt if args.fresh else prompt + LAYOUT_LOCK,
                         reference=None if args.fresh else chosen["file"])
    write_atomic(args.output, data)
    if args.keep_prompt:
        Path(args.output).with_suffix(".prompt.txt").write_text(prompt, encoding="utf-8")
    state["finals"] += 1
    save_state(args.output, state)
    emit(status="ok", step="final", file=args.output, from_draft=pick, seconds=round(time.monotonic() - started),
         budget=budget_view(state),
         next="Check every text string at full size and the arrows. For one local defect: edit -o "
              f"{args.output} -f fix.txt (one edit allowed). Otherwise report the remaining flaws.")


def cmd_edit(args, state):
    if not Path(args.output).is_file() or not state["finals"]:
        raise Refusal("edit fixes a final image; run final first")
    prompt = read_prompt(args)
    if not prompt:
        raise SystemExit("error: edit needs a non-empty --prompt or --prompt-file")
    spend(state, "edits", args.allow_extra)
    directory = state_dir(args.output)
    directory.mkdir(parents=True, exist_ok=True)
    before = directory / f"before-edit-{state['edits'] + 1}{Path(args.output).suffix}"
    before.write_bytes(Path(args.output).read_bytes())
    started = time.monotonic()
    data = request_image(args, model=args.model or FINAL_MODEL, quality=args.quality or FINAL_QUALITY,
                         size=checked_size(args.size) or state["size"], prompt=prompt,
                         fmt=image_format(args.output, args.format), reference=str(before))
    write_atomic(args.output, data)
    state["edits"] += 1
    save_state(args.output, state)
    emit(status="ok", step="edit", file=args.output, previous=str(before),
         seconds=round(time.monotonic() - started), budget=budget_view(state),
         next="Compare with the previous version; keep whichever is better. No further edits without approval.")


def cmd_status(args, state):
    emit(status="ok", step="status", output=args.output, size=state["size"],
         drafts=[d["file"] for d in state["drafts"]], budget=budget_view(state))


def cmd_reset(args, state):
    path = state_dir(args.output) / "state.json"
    if path.exists():
        path.unlink()
    emit(status="ok", step="reset", output=args.output, note="drafts stay on disk; budgets start over")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    commands = {"draft": cmd_draft, "final": cmd_final, "edit": cmd_edit, "status": cmd_status, "reset": cmd_reset}
    helps = {"draft": f"cheap draft ({DRAFT_MODEL}, {DRAFT_QUALITY}, long edge {DRAFT_LONG_EDGE})",
             "final": f"one final render ({FINAL_MODEL}, {FINAL_QUALITY}) locked to a reviewed draft",
             "edit": "one local fix on the final image", "status": "show drafts and budgets",
             "reset": "start a new image at the same path (only for a new image or with user approval)"}
    for name in commands:
        command = sub.add_parser(name, help=helps[name])
        command.add_argument("-o", "--output", required=True, help="final image path; its extension picks the format")
        if name in ("status", "reset"):
            continue
        command.add_argument("-p", "--prompt", help="final: defaults to the draft's prompt")
        command.add_argument("-f", "--prompt-file", help='prompt file, or "-" for stdin')
        if name == "final":
            command.add_argument("--draft", type=int, help="draft number to render (default: latest)")
            command.add_argument("--fresh", action="store_true", help="do not use the draft as layout reference")
            command.add_argument("--keep-prompt", action="store_true", help="save <output>.prompt.txt next to it")
        command.add_argument("-s", "--size", help="final WxH (default 1536x1024); drafts are scaled down")
        command.add_argument("-q", "--quality", choices=["low", "medium", "high", "xhigh", "max"])
        command.add_argument("-m", "--model")
        if name in ("final", "edit"):
            command.add_argument("--format", choices=["png", "webp", "jpeg"],
                                 help="default: from the output extension, else png")
            command.add_argument("--compression", type=int, choices=range(0, 101), metavar="0-100")
        command.add_argument("--allow-extra", action="store_true",
                             help="exceed the budget; only with the user's explicit approval")
        command.add_argument("-b", "--base-url")
        command.add_argument("-k", "--api-key", help="visible in the process list; prefer OPENAI_API_KEY")
    args = parser.parse_args(argv)
    if getattr(args, "compression", None) is not None and image_format(args.output, args.format) == "png":
        parser.error("--compression applies only to webp or jpeg")
    try:
        commands[args.command](args, load_state(args.output))
    except Refusal as refusal:
        emit(status="refused", step=args.command, reason=str(refusal), budget=budget_view(load_state(args.output)))
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
