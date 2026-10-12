#!/usr/bin/env python3
"""Draft cheaply, keep a good draft or render one final image; budgets are enforced per output file.

Every command prints one JSON line. State (drafts, counts, prompt) lives outside the
project in $XDG_STATE_HOME/openai-image-gen/<hash of the output path>/.
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

DRAFT_MODEL, DRAFT_QUALITY, DRAFT_LONG_EDGE = "gpt-image-2.5-flare", "low", 1536
FINAL_MODEL, FINAL_QUALITY = "gpt-image-2.5-sunburst", "high"
BUDGET = {"drafts": 3, "finals": 1, "edits": 1}
MAX_REFS, MAX_REF_BYTES = 8, 20 * 2**20
LAYOUT_LOCK = ("\n\nThe first attached image is the approved draft. Use it only as the layout reference: keep its "
               "composition, element positions and arrows. Render everything at full quality, with crisp, "
               "exactly spelled text as listed above.")


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


# Gateways that relay ChatGPT accounts render every request at low quality; what one
# echoed back is remembered so a final that could only copy its draft is not paid for.
def gateway_notes():
    path = state_root() / "gateways.json"
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


def note_gateway(base_url, **facts):
    notes = gateway_notes()
    notes[base_url] = {**notes.get(base_url, {}), **facts}
    state_root().mkdir(parents=True, exist_ok=True)
    tmp = state_root() / "gateways.json.tmp"
    tmp.write_text(json.dumps(notes, indent=2))
    os.replace(tmp, state_root() / "gateways.json")


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
                f"Content-Type: {IMAGE_TYPES[sniff(Path(path).read_bytes())[0]]}\r\n\r\n")
        parts.append(head.encode() + Path(path).read_bytes() + b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return b"".join(parts), f"multipart/form-data; boundary={boundary}"


def request_image(args, *, model, quality, size, prompt, fmt, references=(), shape=None):
    """The image bytes and what the gateway says it rendered (quality, size, format).

    `shape` is the requested final size, whose orientation the prompt states; a draft's
    scaled size may round off its ratio."""
    base_url, api_key = credentials(args)
    fields = {"model": model, "prompt": f"{prompt.rstrip()}\n\n{orientation(shape or size)}", "size": size,
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
    # A low request echoed as low says nothing about whether higher tiers are honoured.
    if echo.get("quality") and quality not in ("auto", "low"):
        note_gateway(base_url, ignores_quality=echo["quality"] != quality)
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


def returned(data, fmt, size, echo, quality):
    """What the API actually sent back, with a warning when it ignored the request."""
    got, width, height = sniff(data)
    info = {"returned": f"{got} {width}x{height}"}
    warnings = []
    if got != fmt or f"{width}x{height}" != size:
        warnings.append(f"requested {fmt} {size} but got {got} {width}x{height}; the gateway ignored it. "
                        "Use the real size in HTML width/height and convert the format if it matters.")
    if echo.get("quality") and quality != "auto" and echo["quality"] != quality:
        info["quality"] = echo["quality"]
        warnings.append(f"requested quality {quality} but the gateway rendered {echo['quality']}; "
                        "a final from a draft adds nothing here, so keep good drafts instead.")
    if warnings:
        info["warning"] = " ".join(warnings)
    return info


def convert(source, target, fmt):
    """Write `source` as `fmt` at `target` with ffmpeg; False when no converter is installed."""
    if not shutil.which("ffmpeg"):
        return False
    codec = {"webp": ["-c:v", "libwebp", "-quality", "90"], "jpeg": ["-q:v", "2"], "png": []}[fmt]
    tmp = Path(target).with_name(f".{Path(target).name}.{uuid.uuid4().hex[:8]}.{'jpg' if fmt == 'jpeg' else fmt}")
    done = subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(source), *codec, "-frames:v", "1", str(tmp)],
                          capture_output=True)
    if done.returncode != 0 or not tmp.is_file():
        tmp.unlink(missing_ok=True)
        raise SystemExit(f"error: converting the draft to {fmt} failed: {done.stderr.decode(errors='replace')[:300]}")
    os.replace(tmp, target)
    return True


def draft_prompt(entry):
    return Path(entry["file"]).with_name(f"draft-{entry['n']}.prompt.txt").read_text(encoding="utf-8")


def pick_draft(state, number):
    if not state["drafts"]:
        raise Refusal("no draft yet; run draft first and review it")
    pick = number or state["drafts"][-1]["n"]
    chosen = next((d for d in state["drafts"] if d["n"] == pick), None)
    if chosen is None:
        raise Refusal(f"draft {pick} does not exist")
    return pick, chosen


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
    """References recorded with a draft or image, plus a warning for any that changed on disk since."""
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

def cmd_draft(args, state):
    prompt = read_prompt(args)
    if not prompt:
        raise SystemExit("error: draft needs a non-empty --prompt or --prompt-file")
    width, height = parse_size(args.size or state["size"] or "1536x1024")
    refs = reference_entries(args.ref or [])
    digests = [e["sha256"] for e in refs]
    for earlier in state["drafts"]:
        if (earlier.get("size", state["size"]) == f"{width}x{height}" and same_prompt(draft_prompt(earlier), prompt)
                and [e["sha256"] for e in earlier.get("refs", [])] == digests):
            raise Refusal(f"draft {earlier['n']} already used this prompt, size and references ({earlier['file']}); "
                          f"nothing was sent. Change the prompt, or keep or final draft {earlier['n']}.")
    spend(state, "drafts", args.allow_extra)
    state["size"] = f"{width}x{height}"
    number = len(state["drafts"]) + 1
    directory = state_dir(args.output)
    started = time.monotonic()
    size = draft_size(width, height)
    quality = args.quality or DRAFT_QUALITY
    data, echo = request_image(args, model=args.model or DRAFT_MODEL, quality=quality, size=size,
                               prompt=prompt + reference_note(refs, 1), fmt="webp", shape=state["size"],
                               references=reference_view(refs))
    got = sniff(data)[0]
    file = directory / f"draft-{number}.{'jpg' if got == 'jpeg' else got}"
    write_atomic(file, data)
    (directory / f"draft-{number}.prompt.txt").write_text(prompt, encoding="utf-8")
    state["drafts"].append({"n": number, "file": str(file), "size": state["size"], "refs": refs})
    save_state(args.output, state)
    extra = {"references": reference_view(refs)} if refs else {}
    emit(status="ok", step="draft", draft=number, file=str(file), seconds=round(time.monotonic() - started), **extra,
         **returned(data, "webp", size, echo, quality), budget=budget_view(state),
         next=f"Review the draft for layout, arrows and text. If it already looks finished, keep it for free: "
              f"keep -o {args.output} --draft {number}. If only the render quality falls short: final -o "
              f"{args.output} --draft {number}. If the layout is wrong, fix the prompt and draft again.")


def cmd_final(args, state):
    pick, chosen = pick_draft(state, args.draft)
    # A corrected prompt may replace the draft's, e.g. to fix a label the draft got wrong.
    prompt = read_prompt(args) or draft_prompt(chosen)
    base_url = credentials(args)[0]
    if (not args.fresh and same_prompt(prompt, draft_prompt(chosen))
            and gateway_notes().get(base_url, {}).get("ignores_quality")):
        raise Refusal(f"this gateway renders every request at low quality, so a final with draft {pick}'s own prompt "
                      f"would return a copy of it; nothing was sent. Keep it: keep -o {args.output} --draft {pick}; "
                      "or pass a changed prompt, or --fresh for a new composition.")
    # The draft's references carry over, so the final keeps the same characters.
    refs, note = (reference_entries(args.ref), None) if args.ref else inherited_references(chosen.get("refs", []))
    spend(state, "finals", args.allow_extra)
    fmt = image_format(args.output, args.format)
    started = time.monotonic()
    size = checked_size(args.size) or state["size"]
    quality = args.quality or FINAL_QUALITY
    layout = [] if args.fresh else [chosen["file"]]
    data, echo = request_image(args, model=args.model or FINAL_MODEL, quality=quality,
                               size=size, fmt=fmt,
                               prompt=(prompt if args.fresh else prompt + LAYOUT_LOCK) + reference_note(refs, len(layout) + 1),
                               references=layout + reference_view(refs))
    write_atomic(args.output, data)
    if args.keep_prompt:
        Path(args.output).with_suffix(".prompt.txt").write_text(prompt, encoding="utf-8")
    state["finals"] += 1
    state["prompt"], state["refs"] = prompt, refs
    save_state(args.output, state)
    info = returned(data, fmt, size, echo, quality)
    if note:
        info["warning"] = " ".join(filter(None, [info.get("warning"), note]))
    if refs:
        info["references"] = reference_view(refs)
    emit(status="ok", step="final", file=args.output, from_draft=pick, seconds=round(time.monotonic() - started),
         **info, budget=budget_view(state),
         next="Check every text string at full size and the arrows. For one local defect: edit -o "
              f"{args.output} -f fix.txt (one edit allowed). Otherwise report the remaining flaws.")


def cmd_keep(args, state):
    pick, chosen = pick_draft(state, args.draft)
    fmt = image_format(args.output, args.format)
    got = sniff(Path(chosen["file"]).read_bytes())[0]
    if got in (fmt, "unknown"):
        Path(args.output).parent.mkdir(parents=True, exist_ok=True)
        write_atomic(args.output, Path(chosen["file"]).read_bytes())
    elif not convert(chosen["file"], args.output, fmt):
        raise Refusal(f"draft {pick} is {got}, and converting it to {fmt} needs ffmpeg, which is not installed; "
                      f"pass --format {got} to keep it as {got} bytes")
    prompt = draft_prompt(chosen)
    if args.keep_prompt:
        Path(args.output).with_suffix(".prompt.txt").write_text(prompt, encoding="utf-8")
    state["kept"], state["prompt"], state["refs"] = pick, prompt, chosen.get("refs", [])
    save_state(args.output, state)
    emit(status="ok", step="keep", file=args.output, from_draft=pick, **returned(Path(args.output).read_bytes(),
         fmt, state["size"], {}, "auto"), budget=budget_view(state),
         next="Check every text string at full size and the arrows. For one local defect: edit -o "
              f"{args.output} -f fix.txt (one edit allowed). Otherwise report the remaining flaws.")


def cmd_edit(args, state):
    if not Path(args.output).is_file() or not (state["finals"] or state.get("kept")):
        raise Refusal("edit fixes the delivered image; keep a draft or run final first")
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
    quality = args.quality or FINAL_QUALITY
    data, echo = request_image(args, model=args.model or FINAL_MODEL, quality=quality, size=size,
                               prompt=prompt + reference_note(refs, 2), fmt=fmt,
                               references=[str(before)] + reference_view(refs))
    write_atomic(args.output, data)
    state["edits"] += 1
    save_state(args.output, state)
    info = returned(data, fmt, size, echo, quality)
    if note:
        info["warning"] = " ".join(filter(None, [info.get("warning"), note]))
    emit(status="ok", step="edit", file=args.output, previous=str(before),
         seconds=round(time.monotonic() - started), **info, budget=budget_view(state),
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
    commands = {"draft": cmd_draft, "keep": cmd_keep, "final": cmd_final, "edit": cmd_edit,
                "status": cmd_status, "reset": cmd_reset}
    helps = {"draft": f"cheap draft ({DRAFT_MODEL}, {DRAFT_QUALITY}, long edge {DRAFT_LONG_EDGE})",
             "keep": "deliver a reviewed draft as the image; no call, no cost",
             "final": f"one final render ({FINAL_MODEL}, {FINAL_QUALITY}) locked to a reviewed draft",
             "edit": "one local fix on the final image", "status": "show drafts and budgets",
             "reset": "start a new image at the same path (only for a new image or with user approval)"}
    for name in commands:
        command = sub.add_parser(name, help=helps[name])
        command.add_argument("-o", "--output", required=True, help="final image path; its extension picks the format")
        if name in ("status", "reset"):
            continue
        if name == "keep":
            command.add_argument("--draft", type=int, help="draft number to keep (default: latest)")
            command.add_argument("--format", choices=["png", "webp", "jpeg"],
                                 help="default: from the output extension, else png")
            command.add_argument("--keep-prompt", action="store_true", help="save <output>.prompt.txt next to it")
            continue
        command.add_argument("-p", "--prompt", help="final: defaults to the draft's prompt")
        command.add_argument("-f", "--prompt-file", help='prompt file, or "-" for stdin')
        if name == "final":
            command.add_argument("--draft", type=int, help="draft number to render (default: latest)")
            command.add_argument("--fresh", action="store_true", help="do not use the draft as layout reference")
            command.add_argument("--keep-prompt", action="store_true", help="save <output>.prompt.txt next to it")
        command.add_argument("-r", "--ref", action="append", metavar="IMAGE",
                             help=f"reference image of a subject to keep consistent, e.g. a character sheet "
                                  f"(repeatable, at most {MAX_REFS}); final and edit default to the draft's")
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
