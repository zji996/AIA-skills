#!/usr/bin/env python3
"""Render one image straight to its output file, fix it only when it is clearly wrong.

Every command prints one JSON line. State (earlier renders, counts, prompt) lives outside
the project in $XDG_STATE_HOME/openai-image-gen/<hash of the output path>/.
"""
import argparse
import base64
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
import uuid
from pathlib import Path

MODEL, QUALITY, SIZE = "gpt-image-2.6-sunburst", "medium", "1536x1024"
BUDGET = {"images": 3, "edits": 1}
MAX_REFS, MAX_REF_BYTES = 8, 20 * 2**20


# The edits endpoint rejects parts without an image MIME type.
IMAGE_TYPES = {"png": "image/png", "webp": "image/webp", "jpeg": "image/jpeg", "unknown": "image/png"}


class Refusal(Exception):
    """A request the budget or workflow does not allow; exit code 3."""


def emit(**fields):
    print(json.dumps(fields, ensure_ascii=False))


def state_root():
    return Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "openai-image-gen"


def state_dir(output):
    return state_root() / hashlib.sha256(str(Path(output).resolve()).encode()).hexdigest()[:16]


def same_prompt(left, right):
    return bool(left) and bool(right) and " ".join(left.split()) == " ".join(right.split())


def load_state(output):
    path = state_dir(output) / "state.json"
    state = json.loads(path.read_text()) if path.is_file() else {}
    if "images" not in state:  # new path, or state from the 2.x draft/final workflow
        state = {"output": str(Path(output).resolve()), "size": None, "images": [], "edits": 0}
    return state


def save_state(output, state):
    directory = state_dir(output)
    directory.mkdir(parents=True, exist_ok=True)
    tmp = directory / "state.json.tmp"
    tmp.write_text(json.dumps(state, ensure_ascii=False, indent=2))
    os.replace(tmp, directory / "state.json")


def budget_view(state):
    return {"images": f"{len(state['images'])}/{BUDGET['images']}",
            "edits": f"{state['edits']}/{BUDGET['edits']}"}


def spend(state, kind, allow_extra):
    used = len(state["images"]) if kind == "images" else state[kind]
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


def orientation(size):
    """Some gateways ignore `size` and take the shape from the prompt, so the prompt says it too."""
    width, height = parse_size(size)
    if width == height:
        return "Square 1:1 composition."
    divisor = math.gcd(width, height)
    w, h = width // divisor, height // divisor
    if max(w, h) > 32:
        long = f"{max(width, height) / min(width, height):.2f}".rstrip("0").rstrip(".")
        w, h = (long, 1) if width > height else (1, long)
    if width > height:
        return f"Landscape orientation, {w}:{h} aspect ratio, wider than tall."
    return f"Portrait orientation, {w}:{h} aspect ratio, taller than wide."


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
                f"Content-Type: {IMAGE_TYPES[sniff(Path(path).read_bytes())[0]]}\r\n\r\n")
        parts.append(head.encode() + Path(path).read_bytes() + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def request_image(args, *, model, quality, size, prompt, fmt, references=()):
    """The image bytes and what the gateway says it rendered (quality, size, format)."""
    base_url, api_key = credentials(args)
    fields = {"model": model, "prompt": f"{prompt.rstrip()}\n\n{orientation(size)}", "size": size,
              "quality": quality, "output_format": fmt, "n": 1}
    if getattr(args, "compression", None) is not None:
        fields["output_compression"] = args.compression
    if references:
        body, content_type = multipart(fields, [("image[]", path) for path in references])
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
            error = detail.get("error") if isinstance(detail.get("error"), dict) else detail
            code = error.get("code") or error.get("type")
            message = error.get("message")
            # The code tells a content-policy refusal (moderation_blocked) from a bad request.
            message = ": ".join(str(part) for part in (code, message) if part) or None
        except (ValueError, AttributeError):
            message = None
        raise SystemExit(f"error: image request failed (HTTP {exc.code}): {message or exc.reason}")
    except (urllib.error.URLError, TimeoutError, ValueError) as exc:
        raise SystemExit(f"error: image request failed: {exc}")
    echo = {key: payload[key] for key in ("quality", "size", "output_format") if payload.get(key)}
    item = (payload.get("data") or [{}])[0]
    if item.get("b64_json"):
        try:
            return base64.b64decode(item["b64_json"], validate=True), echo
        except ValueError:
            raise SystemExit("error: invalid base64 image data")
    if str(item.get("url", "")).startswith("https://"):
        try:
            with urllib.request.urlopen(item["url"], timeout=120) as response:
                return response.read(), echo
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


def sniff(data):
    """(format, width, height) read from the image header; gateways may ignore size and format."""
    if data[:8] == b"\x89PNG\r\n\x1a\n" and len(data) >= 24:
        return "png", int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP" and len(data) >= 30:
        chunk = data[12:16]
        if chunk == b"VP8X":
            return "webp", int.from_bytes(data[24:27], "little") + 1, int.from_bytes(data[27:30], "little") + 1
        if chunk == b"VP8L":
            bits = int.from_bytes(data[21:25], "little")
            return "webp", (bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1
        return "webp", int.from_bytes(data[26:28], "little") & 0x3FFF, int.from_bytes(data[28:30], "little") & 0x3FFF
    if data[:2] == b"\xff\xd8":
        index = 2
        while index + 9 < len(data) and data[index] == 0xFF:
            marker, length = data[index + 1], int.from_bytes(data[index + 2:index + 4], "big")
            if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
                return "jpeg", int.from_bytes(data[index + 7:index + 9], "big"), int.from_bytes(data[index + 5:index + 7], "big")
            index += 2 + length
        return "jpeg", 0, 0
    return "unknown", 0, 0


def returned(data, fmt, size, echo, quality, converted):
    """What the API actually sent back, with a warning when it ignored the request."""
    got, width, height = sniff(data)
    info = {"returned": f"{got} {width}x{height}"}
    warnings = []
    if got != fmt or f"{width}x{height}" != size:
        warnings.append(f"requested {fmt} {size} but got {got} {width}x{height}; the gateway ignored it. "
                        + (f"The file was converted to {fmt}. " if converted else
                           f"The file is {got} as sent (no ffmpeg to convert it). " if got != fmt else "")
                        + "Use the real size in HTML width/height.")
    if echo.get("quality") and quality != "auto" and echo["quality"] != quality:
        info["quality"] = echo["quality"]
        warnings.append(f"requested quality {quality} but the gateway rendered {echo['quality']}; "
                        "this gateway picks the quality itself, so asking for a higher one changes nothing.")
    if warnings:
        info["warning"] = " ".join(warnings)
    return info


def convert(source, target, fmt):
    """Write `source` as `fmt` at `target` with ffmpeg; False when no converter is installed."""
    if not shutil.which("ffmpeg"):
        return False
    codec = {"webp": ["-c:v", "libwebp", "-quality", "90"], "jpeg": ["-q:v", "2"], "png": []}[fmt]
    Path(target).parent.mkdir(parents=True, exist_ok=True)
    tmp = Path(target).with_name(f".{Path(target).name}.{uuid.uuid4().hex[:8]}.{'jpg' if fmt == 'jpeg' else fmt}")
    done = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(source), *codec, "-frames:v", "1", str(tmp)],
                          capture_output=True)
    if done.returncode != 0 or not tmp.is_file():
        tmp.unlink(missing_ok=True)
        raise SystemExit(f"error: converting the image to {fmt} failed: {done.stderr.decode(errors='replace')[:300]}")
    os.replace(tmp, target)
    return True


def reference_entries(paths):
    """Validated --ref images with their digests, so a changed file is noticed later."""
    if len(paths) > MAX_REFS:
        raise SystemExit(f"error: at most {MAX_REFS} reference images")
    entries = []
    for value in paths:
        path = Path(value)
        if not path.is_file():
            raise SystemExit(f"error: reference image not found: {value}")
        data = path.read_bytes()
        if sniff(data)[0] == "unknown":
            raise SystemExit(f"error: reference must be a png, webp or jpeg image: {value}")
        if len(data) > MAX_REF_BYTES:
            raise SystemExit(f"error: reference image over {MAX_REF_BYTES >> 20} MB: {value}")
        entries.append({"file": str(path.resolve()), "sha256": hashlib.sha256(data).hexdigest()})
    return entries


def inherited_references(entries):
    """References recorded with an earlier image, plus a warning for any that changed on disk since."""
    changed = [e["file"] for e in entries
               if not Path(e["file"]).is_file() or hashlib.sha256(Path(e["file"]).read_bytes()).hexdigest() != e["sha256"]]
    missing = [f for f in changed if not Path(f).is_file()]
    if missing:
        raise Refusal(f"reference images used before are gone: {', '.join(missing)}; pass --ref again")
    note = f"reference images changed since they were used: {', '.join(changed)}" if changed else None
    return entries, note


def reference_note(entries, first):
    """Prompt text that tells the model what the attached reference images are for."""
    if not entries:
        return ""
    names = ", ".join(f"image {first + i} ({Path(e['file']).parent.name}/{Path(e['file']).name})"
                      for i, e in enumerate(entries))
    return (f"\n\nReference images: {names}. They show subjects that must stay consistent: keep each one's "
            "identity exactly as shown (face, hairstyle, body shape, outfit, colours and signature items). Take "
            "composition, pose, camera, lighting and background only from the description above; do not copy "
            "a reference's layout, plain background, labels or multiple views.")


def reference_view(entries):
    return [e["file"] for e in entries]


# --- commands ---------------------------------------------------------------------------

def image_prompt(entry):
    return Path(entry["prompt_file"]).read_text(encoding="utf-8")


def deliver(args, data, fmt, stem):
    """Write the render to the output (converted when the gateway sent another format) and keep a copy."""
    got = sniff(data)[0]
    copy = state_dir(args.output) / f"{stem}.{'jpg' if got == 'jpeg' else 'png' if got == 'unknown' else got}"
    write_atomic(copy, data)
    converted = got not in (fmt, "unknown") and convert(copy, args.output, fmt)
    if not converted:
        write_atomic(args.output, data)
    return copy, converted


def cmd_image(args, state):
    prompt = read_prompt(args)
    if not prompt:
        raise SystemExit("error: image needs a non-empty --prompt or --prompt-file")
    size = checked_size(args.size) or state["size"] or SIZE
    # Another try at the same image keeps its references unless told otherwise.
    if args.ref:
        refs, note = reference_entries(args.ref), None
    elif args.no_refs:
        refs, note = [], None
    else:
        refs, note = inherited_references(state["images"][-1].get("refs", []) if state["images"] else [])
    digests = [e["sha256"] for e in refs]
    for earlier in state["images"]:
        if (earlier["size"] == size and same_prompt(image_prompt(earlier), prompt)
                and [e["sha256"] for e in earlier.get("refs", [])] == digests):
            raise Refusal(f"image {earlier['n']} already used this prompt, size and references ({earlier['file']}); "
                          "nothing was sent. Change the prompt to fix what is wrong, or edit the image.")
    spend(state, "images", args.allow_extra)
    fmt = image_format(args.output, args.format)
    number = len(state["images"]) + 1
    previous = state["images"][-1]["file"] if state["images"] and Path(args.output).is_file() else None
    started = time.monotonic()
    quality = args.quality or QUALITY
    data, echo = request_image(args, model=args.model or MODEL, quality=quality, size=size, fmt=fmt,
                               prompt=prompt + reference_note(refs, 1), references=reference_view(refs))
    copy, converted = deliver(args, data, fmt, f"image-{number}")
    prompt_file = state_dir(args.output) / f"image-{number}.prompt.txt"
    prompt_file.write_text(prompt, encoding="utf-8")
    if args.keep_prompt:
        Path(args.output).with_suffix(".prompt.txt").write_text(prompt, encoding="utf-8")
    state["images"].append({"n": number, "file": str(copy), "prompt_file": str(prompt_file), "size": size,
                            "refs": refs})
    state["size"], state["prompt"], state["refs"] = size, prompt, refs
    save_state(args.output, state)
    info = returned(data, fmt, size, echo, quality, converted)
    if note:
        info["warning"] = " ".join(filter(None, [info.get("warning"), note]))
    if refs:
        info["references"] = reference_view(refs)
    if previous:
        info["previous"] = previous
    emit(status="ok", step="image", image=number, file=args.output, seconds=round(time.monotonic() - started),
         **info, budget=budget_view(state),
         next=f"{args.output} is the finished image. Every further call costs as much as this one: edit "
              "changes a stated detail and redraws the rest faithfully, image with a revised prompt starts over.")


def cmd_edit(args, state):
    if not Path(args.output).is_file() or not state["images"]:
        raise Refusal("edit fixes an image made here; run image first")
    prompt = read_prompt(args)
    if not prompt:
        raise SystemExit("error: edit needs a non-empty --prompt or --prompt-file")
    if same_prompt(prompt, state.get("prompt")):
        raise Refusal("an edit must say what to change; this is the prompt the image was made from, "
                      "and the gateway would return a copy. Nothing was sent.")
    refs, note = (reference_entries(args.ref), None) if args.ref else inherited_references(state.get("refs", []))
    spend(state, "edits", args.allow_extra)
    directory = state_dir(args.output)
    directory.mkdir(parents=True, exist_ok=True)
    before = directory / f"before-edit-{state['edits'] + 1}{Path(args.output).suffix}"
    before.write_bytes(Path(args.output).read_bytes())
    started = time.monotonic()
    size, fmt = checked_size(args.size) or state["size"], image_format(args.output, args.format)
    quality = args.quality or QUALITY
    data, echo = request_image(args, model=args.model or MODEL, quality=quality, size=size,
                               prompt=prompt + reference_note(refs, 2), fmt=fmt,
                               references=[str(before)] + reference_view(refs))
    converted = deliver(args, data, fmt, f"edit-{state['edits'] + 1}")[1]
    state["edits"] += 1
    save_state(args.output, state)
    info = returned(data, fmt, size, echo, quality, converted)
    if note:
        info["warning"] = " ".join(filter(None, [info.get("warning"), note]))
    emit(status="ok", step="edit", file=args.output, previous=str(before),
         seconds=round(time.monotonic() - started), **info, budget=budget_view(state),
         next="The version before the edit is at previous; restore it if this one is worse.")


def cmd_status(args, state):
    emit(status="ok", step="status", output=args.output, size=state["size"],
         images=[d["file"] for d in state["images"]], budget=budget_view(state))


def cmd_reset(args, state):
    path = state_dir(args.output) / "state.json"
    if path.exists():
        path.unlink()
    emit(status="ok", step="reset", output=args.output, note="earlier renders stay on disk; budgets start over")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    commands = {"image": cmd_image, "edit": cmd_edit, "status": cmd_status, "reset": cmd_reset}
    helps = {"image": f"render the image to its output ({MODEL}, {QUALITY})",
             "edit": "change a stated detail of the image, keeping the rest", "status": "show renders and budgets",
             "reset": "start a new image at the same path (only for a new image or with user approval)"}
    for name in commands:
        command = sub.add_parser(name, help=helps[name])
        command.add_argument("-o", "--output", required=True, help="image path; its extension picks the format")
        if name in ("status", "reset"):
            continue
        command.add_argument("-p", "--prompt")
        command.add_argument("-f", "--prompt-file", help='prompt file, or "-" for stdin')
        command.add_argument("-r", "--ref", action="append", metavar="IMAGE",
                             help=f"reference image of a subject to keep consistent, e.g. a character sheet "
                                  f"(repeatable, at most {MAX_REFS}); defaults to the previous image's")
        if name == "image":
            command.add_argument("--no-refs", action="store_true", help="drop the previous image's references")
            command.add_argument("--keep-prompt", action="store_true", help="save <output>.prompt.txt next to it")
        command.add_argument("-s", "--size", help=f"WxH (default {SIZE}, or this image's size)")
        command.add_argument("-q", "--quality", choices=["low", "medium", "high", "xhigh", "max"],
                             help=f"default {QUALITY}; high only when fine detail or small text needs it")
        command.add_argument("-m", "--model", help=f"default {MODEL}; gpt-image-2.6-flare is faster")
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
