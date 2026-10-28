# /// script
# requires-python = ">=3.13"
# ///
"""Voices the site's spoken lines (audio/README.md): every line of the chosen pages that is not cached yet goes to one
warm Fish Audio Drama 3 server (the voice app's tools/tts/speak.py, MLX, Apple Silicon only), comes back as a WAV with
word timings (align.py's Qwen3-ForcedAligner, inside speak.py), is loudness-normalised and encoded to Opus in WebM
with ffmpeg, and lands in .audio/files/ under the hash of its bytes. Then each page whose lines are all voiced gets
audio/manifests/<doc>.json, which the build (gen/build.ts) turns into the page's player.

    uv run --script audio/generate.py                         # from site/: every page
    uv run --script audio/generate.py /charter /rfcs/linear-types
    uv run --script audio/generate.py --dry-run               # what would be generated, nothing run
    uv run --script audio/generate.py --check /charter        # Whisper transcripts of a page's audio against its text

The lines come from `.gen/spoken.json`, which `bun gen/build.ts` writes (run first, unless --no-build). Before
synthesis each line goes through audio/lexicon.toml (how to say rustc, blaze, 1.99.0, ...): the model gets the spoken
form, the manifest keeps the page's words, each with the merged timing of the spoken words it became. A line is
generated once per cache key: sha256 of (spoken text, voice reference, model revision, engine, quantisation, seed), so
a lexicon edit regenerates only the lines it changes; the seed is fixed per line (from its page text), so a rerun on an
empty cache asks the model for the same take. A take whose length
is far from what its words need (a stall or a ramble) is redone with the next seed, up to three times, and the seed
used is recorded.

Needs: uv, ffmpeg with libopus (`brew install ffmpeg`), the voice repo's tools/tts with its checkpoint (VOICE_TTS or
--tts, default /Volumes/Projects/indexable-inc/voice/tools/tts), and a reference voice dir with ref.wav and ref.txt
(--voice, default the voice app's hifi-92). About 50 GB of memory while it runs, roughly 0.7x real time on an M5 Max.
"""

import argparse
import asyncio
import hashlib
import json
import os
import re
import shutil
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

SITE = Path(__file__).resolve().parent.parent
CACHE = SITE / ".audio"
FILES = CACHE / "files"
ENTRIES = CACHE / "cache"
MANIFESTS = SITE / "audio" / "manifests"
SPOKEN = SITE / ".gen" / "spoken.json"
LEXICON = SITE / "audio" / "lexicon.toml"
DEFAULT_TTS = Path(os.environ.get("VOICE_TTS", "/Volumes/Projects/indexable-inc/voice/tools/tts"))
DEFAULT_VOICE = Path("~/Library/Application Support/voice/voices/hifi-92").expanduser()
EXPERTS = "q8"
# Opus in WebM: Chrome, Firefox, and Safari 17.4+ on macOS and iOS (audio/README.md). Mono speech at 32 kb/s VBR.
# Loudness to -18 LUFS (two-pass loudnorm, linear), so lines sit at one level whatever their direction.
ENCODE = ["-ac", "1", "-c:a", "libopus", "-b:a", "32k", "-vbr", "on", "-application", "audio"]
LOUDNESS = "I=-18:TP=-2:LRA=11"
ENCODING_ID = "opus32k-webm-loudnorm18-v1"
TAKES = 3
# Spoken English runs at about 2.5 words a second; a take outside these bounds of that is redone.
WORDS_PER_S = 2.5
TOO_SHORT, TOO_LONG = 0.4, 2.6


class StepError(Exception):
    pass


async def run(step: str, *cmd: str, bound: float = 300, stdin: bytes | None = None, cwd: Path | None = None) -> bytes:
    """Runs `cmd` to completion; on failure names the step, the command, its exit code and the stderr tail."""
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(bound):
            out, err = await proc.communicate(stdin)
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise StepError(f"{step}: {' '.join(cmd)}: no exit after {bound:.0f} s") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-25:])
        raise StepError(f"{step}: {' '.join(cmd)}: exit {proc.returncode}\n{tail}")
    return out


async def run_err(step: str, *cmd: str, bound: float = 300) -> str:
    """Like `run`, but returns stderr (ffmpeg prints loudnorm's measurement there)."""
    proc = await asyncio.create_subprocess_exec(
        *cmd, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE
    )
    try:
        async with asyncio.timeout(bound):
            _, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise StepError(f"{step}: {' '.join(cmd)}: no exit after {bound:.0f} s") from None
    text = err.decode(errors="replace")
    if proc.returncode != 0:
        raise StepError(f"{step}: {' '.join(cmd)}: exit {proc.returncode}\n" + "\n".join(text.splitlines()[-25:]))
    return text


def sha(*parts: bytes | str) -> str:
    h = hashlib.sha256()
    for p in parts:
        h.update(p.encode() if isinstance(p, str) else p)
        h.update(b"\0")
    return h.hexdigest()


# `[warm]`, `[excited, quick]`, `<whisper>`, `</whisper>`: directions for the voice, never words (align.py's rule).
TAG = re.compile(r"\[[^\]]*\]|</?[a-z]{2,12}>")


def speakable(text: str) -> str:
    """The words of a line: Drama 3 directions dropped (align.py's rule)."""
    return " ".join(TAG.sub(" ", text).split())


def has_word(token: str) -> bool:
    """Whether the aligner times this token (align.py keeps tokens with a letter or digit)."""
    return any(c.isalnum() for c in token)


@dataclass(frozen=True)
class Lexicon:
    """audio/lexicon.toml: how to say a word the voice gets wrong. `words` are whole-word and case-sensitive (a word
    edge is anything but a letter, digit or underscore, so `blaze's` and `(rustc)` match, `rustcx` does not); `rules`
    are regexes applied after them. Both work inside one whitespace-separated word, and never inside a [direction]."""

    table: dict[str, str]
    words: re.Pattern[str] | None
    folded: re.Pattern[str] | None  # the same words, any case: only for normalising a Whisper transcript
    rules: tuple[tuple[re.Pattern[str], str], ...]

    @staticmethod
    def load(path: Path = LEXICON) -> "Lexicon":
        try:
            data = tomllib.loads(path.read_text())
        except FileNotFoundError:
            raise StepError(f"lexicon: {path} is missing") from None
        except tomllib.TOMLDecodeError as e:
            raise StepError(f"lexicon: {path}: {e}") from None
        table: dict[str, str] = {}
        for k, v in (data.get("words") or {}).items():
            if not isinstance(v, str) or not k or any(c.isspace() for c in k):
                raise StepError(f"lexicon: {path}: words.{k!r}: the key is one word and the value a string")
            Lexicon._check_spoken(path, f"words.{k!r}", v)
            table[k] = v
        rules = []
        for i, r in enumerate(data.get("rules") or []):
            where = f"rules[{i}] ({r.get('name', '?')})"
            if not isinstance(r.get("pattern"), str) or not isinstance(r.get("spoken"), str):
                raise StepError(f"lexicon: {path}: {where}: needs pattern and spoken strings")
            try:
                rules.append((re.compile(r["pattern"]), r["spoken"]))
            except re.error as e:
                raise StepError(f"lexicon: {path}: {where}: pattern: {e}") from None
            Lexicon._check_spoken(path, where, r["spoken"])
        alts = "|".join(re.escape(k) for k in sorted(table, key=lambda k: (-len(k), k)))
        edge = rf"(?<!\w)(?:{alts})(?!\w)"
        return Lexicon(
            table=table,
            words=re.compile(edge) if table else None,
            folded=re.compile(edge, re.IGNORECASE) if table else None,
            rules=tuple(rules),
        )

    @staticmethod
    def _check_spoken(path: Path, where: str, spoken: str) -> None:
        if not has_word(spoken) or any(c in spoken for c in "[]<>"):
            raise StepError(f"lexicon: {path}: {where}: {spoken!r} must have a word and no [ ] < > (directions)")

    def word(self, token: str, fold: bool = False) -> str:
        """How one display word (no whitespace) is said."""
        pattern = self.folded if fold else self.words
        if pattern is not None:
            lower = {k.lower(): v for k, v in self.table.items()} if fold else {}
            token = pattern.sub(lambda m: lower[m.group().lower()] if fold else self.table[m.group()], token)
        for pat, spoken in self.rules:
            token = pat.sub(spoken, token)
        return token

    def say(self, text: str, fold: bool = False) -> str:
        """The line as the voice gets it: every word outside the directions through `word`, directions and spacing as
        they were."""
        out, at = [], 0
        for m in [*TAG.finditer(text), None]:
            end = m.start() if m else len(text)
            out.append(re.sub(r"\S+", lambda w: self.word(w.group(), fold), text[at:end]))
            if m:
                out.append(m.group())
                at = m.end()
        return "".join(out)

    def display_words(self, text: str, words: list) -> list:
        """The aligner's timings of the spoken line (one per spoken word) as timings of the page's words: each display
        word gets the span from its first spoken word's start to its last one's end. listen.ts matches these words
        against the page text, so they must be the page's words. If the aligner fell back to its own words (its count
        differs), they are kept as they are."""
        counts = [(tok, sum(has_word(w) for w in self.word(tok).split())) for tok in speakable(text).split()]
        counts = [(tok, n) for tok, n in counts if n]
        if len(words) != sum(n for _, n in counts):
            if self.say(text) != text:
                print(f"lexicon: {len(words)} timed words for {sum(n for _, n in counts)} spoken in {text[:60]!r}: "
                      "kept the aligner's", file=sys.stderr)
            return words
        out, i = [], 0
        for tok, n in counts:
            out.append([tok, words[i][1], words[i + n - 1][2]])
            i += n
        return out


@dataclass(frozen=True)
class Identity:
    """What a take depends on besides its text and seed."""

    voice: str  # sha256 of ref.wav and ref.txt
    model: str  # Drama 3 checkpoint revision + drama3-mlx revision
    engine: str  # sha256 of stream.py, the sampling and streaming code
    experts: str

    def key(self, text: str, seed: int) -> str:
        return sha("line-v1", text, self.voice, self.model, self.engine, self.experts, str(seed))


def identity(tts: Path, voice: Path) -> Identity:
    for f in (voice / "ref.wav", voice / "ref.txt"):
        if not f.exists():
            raise StepError(f"voice: {f} is missing (a voice dir holds ref.wav and ref.txt)")
    meta = tts / "checkpoints" / "drama-3" / ".cache" / "huggingface" / "download" / "config.json.metadata"
    if not meta.exists():
        raise StepError(f"model: {meta} is missing (download the checkpoint as {tts}/README.md says)")
    revision = meta.read_text().split()[0]
    lock = (tts / "uv.lock").read_text()
    m = re.search(r"drama3-mlx\?rev=([0-9a-f]{40})", lock)
    if not m:
        raise StepError(f"model: no drama3-mlx revision in {tts}/uv.lock")
    return Identity(
        voice=sha((voice / "ref.wav").read_bytes(), (voice / "ref.txt").read_text().strip()),
        model=f"drama-3@{revision}+drama3-mlx@{m.group(1)}",
        engine=sha((tts / "stream.py").read_bytes())[:16],
        experts=EXPERTS,
    )


def seed_of(text: str) -> int:
    """The line's fixed seed: from its text, so it never depends on order or on other lines."""
    return int(sha("seed", text)[:8], 16) & 0x7FFFFFFF


@dataclass
class Line:
    page: str
    id: str
    block: str
    text: str  # as on the page (and in the manifest)
    spoken: str  # through the lexicon: what the model gets
    seed: int
    key: str


@dataclass
class Stats:
    generated: int = 0
    cached: int = 0
    retakes: int = 0
    audio_s: float = 0.0
    gen_s: float = 0.0
    failed: list[str] = field(default_factory=list)


class Server:
    """One speak.py --serve child, model loaded once; requests go in as JSON lines, results come back by id."""

    def __init__(self, tts: Path, voice: Path):
        self.tts, self.voice = tts, voice
        self.proc: asyncio.subprocess.Process | None = None
        self.waiting: dict[int, asyncio.Future[dict]] = {}
        self.next_id = 1
        self.tail: list[str] = []
        self.tasks: list[asyncio.Task] = []

    async def start(self, bound: float = 900) -> dict:
        uv = shutil.which("uv")
        if not uv:
            raise StepError("tts: uv is not on PATH")
        cmd = [uv, "run", "--directory", str(self.tts), "speak.py", "--serve", "--experts", EXPERTS, "--voice", str(self.voice)]
        self.proc = await asyncio.create_subprocess_exec(
            *cmd, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            limit=1 << 24,
        )
        self.cmd = " ".join(cmd)
        ready: asyncio.Future[dict] = asyncio.get_running_loop().create_future()
        self.tasks = [asyncio.create_task(self._stdout(ready)), asyncio.create_task(self._stderr())]
        try:
            async with asyncio.timeout(bound):
                return await ready
        except TimeoutError:
            raise StepError(f"tts: {self.cmd}: not ready after {bound:.0f} s\n" + "\n".join(self.tail)) from None

    async def _stdout(self, ready: asyncio.Future[dict]) -> None:
        assert self.proc and self.proc.stdout
        while line := await self.proc.stdout.readline():
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("ready") and not ready.done():
                ready.set_result(msg)
            elif "chunk" in msg or "upto" in msg:
                continue
            elif (f := self.waiting.pop(msg.get("id"), None)) is not None and not f.done():
                f.set_result(msg)
        code = await self.proc.wait()
        err = StepError(f"tts: {self.cmd}: exit {code}\n" + "\n".join(self.tail))
        if not ready.done():
            ready.set_exception(err)
        for f in self.waiting.values():
            if not f.done():
                f.set_exception(err)

    async def _stderr(self) -> None:
        assert self.proc and self.proc.stderr
        while line := await self.proc.stderr.readline():
            text = line.decode(errors="replace").rstrip()
            self.tail = [*self.tail, text][-25:]
            print(f"[tts] {text}", file=sys.stderr)

    async def speak(self, text: str, out: Path, seed: int, bound: float) -> dict:
        assert self.proc and self.proc.stdin
        rid = self.next_id
        self.next_id += 1
        fut: asyncio.Future[dict] = asyncio.get_running_loop().create_future()
        self.waiting[rid] = fut
        req = {"id": rid, "text": text, "out": str(out), "seed": seed, "align": True}
        self.proc.stdin.write((json.dumps(req) + "\n").encode())
        await self.proc.stdin.drain()
        try:
            async with asyncio.timeout(bound):
                res = await fut
        except TimeoutError:
            raise StepError(f"tts: line {rid}: no result after {bound:.0f} s\n" + "\n".join(self.tail)) from None
        if not res.get("ok"):
            raise StepError(f"tts: line {rid}: {res.get('error') or res}")
        return res

    async def stop(self) -> None:
        if self.proc and self.proc.returncode is None:
            assert self.proc.stdin
            self.proc.stdin.close()
            try:
                async with asyncio.timeout(30):
                    await self.proc.wait()
            except TimeoutError:
                self.proc.kill()
                await self.proc.wait()
        for t in self.tasks:
            t.cancel()


async def encode(wav: Path, ffmpeg: str) -> tuple[bytes, float]:
    """The WAV as Opus in WebM at -18 LUFS, bit-exact for the same input; and its duration in seconds."""
    measured = await run_err(
        "loudnorm measure", ffmpeg, "-hide_banner", "-nostdin", "-i", str(wav),
        "-af", f"loudnorm={LOUDNESS}:print_format=json", "-f", "null", "-",
    )
    m = json.loads(measured[measured.rindex("{"): measured.rindex("}") + 1])
    gain = (
        f"loudnorm={LOUDNESS}:measured_I={m['input_i']}:measured_TP={m['input_tp']}:measured_LRA={m['input_lra']}"
        f":measured_thresh={m['input_thresh']}:offset={m['target_offset']}:linear=true,aresample=48000"
    )
    with tempfile.TemporaryDirectory() as d:
        out = Path(d) / "line.webm"
        await run(
            "encode", ffmpeg, "-hide_banner", "-nostdin", "-loglevel", "error", "-i", str(wav), "-af", gain, *ENCODE,
            "-map_metadata", "-1", "-fflags", "+bitexact", "-flags:a", "+bitexact", "-f", "webm", str(out),
        )
        data = out.read_bytes()
    probe = await run(
        "ffprobe", shutil.which("ffprobe") or "ffprobe", "-v", "error", "-show_entries", "format=duration",
        "-of", "csv=p=0", str(wav),
    )
    return data, round(float(probe.decode().strip()), 3)


def plausible(text: str, audio_s: float) -> bool:
    want = max(len(speakable(text).split()) / WORDS_PER_S, 0.6)
    return TOO_SHORT * want <= audio_s <= TOO_LONG * want + 1.0


async def voice_line(server: Server, line: Line, ffmpeg: str, stats: Stats, tmp: Path) -> dict:
    """Generates, checks, encodes and caches one line; returns its cache entry."""
    words = len(speakable(line.spoken).split())
    res: dict = {}
    seed = line.seed
    for take in range(TAKES):
        seed = line.seed + take
        wav = tmp / f"{line.key[:16]}-{take}.wav"
        res = await server.speak(line.spoken, wav, seed, bound=120 + words * 3.0)
        stats.gen_s += res.get("gen_s", 0.0)
        if plausible(line.spoken, res["audio_s"]):
            break
        stats.retakes += 1
        print(f"retake {line.page} {line.id}: {res['audio_s']} s for {words} words (seed {seed})", file=sys.stderr)
    data, duration = await encode(Path(res["out"]), ffmpeg)
    name = sha(data)[:20] + ".webm"
    FILES.mkdir(parents=True, exist_ok=True)
    (FILES / name).write_bytes(data)
    entry = {
        "key": line.key,
        "text": line.spoken,
        "seed": seed,
        "file": name,
        "duration": duration,
        # The spoken words' timings; manifest() maps them to the page's words.
        "words": [[w, round(s, 3), round(e, 3)] for w, s, e in res.get("words") or []],
        "gen_s": res.get("gen_s"),
        "encoding": ENCODING_ID,
    }
    ENTRIES.mkdir(parents=True, exist_ok=True)
    (ENTRIES / f"{line.key}.json").write_text(json.dumps(entry, indent=1))
    stats.generated += 1
    stats.audio_s += duration
    return entry


def cached(key: str) -> dict | None:
    f = ENTRIES / f"{key}.json"
    if not f.exists():
        return None
    entry = json.loads(f.read_text())
    if entry.get("encoding") != ENCODING_ID or not (FILES / entry["file"]).exists():
        return None
    return entry


def manifest(page: dict, entries: dict[str, dict], who: Identity, voice: Path, lex: Lexicon) -> dict:
    lines = []
    for l in page["lines"]:
        e = entries[l["id"]]
        words = lex.display_words(l["text"], e["words"])
        lines.append({"id": l["id"], "block": l["block"], "src": e["file"], "duration": e["duration"], "words": words})
    licence = (voice / "LICENSE.txt").read_text() if (voice / "LICENSE.txt").exists() else ""
    attribution = next((ln.split(":", 1)[1].strip() for ln in licence.splitlines() if ln.startswith("Attribution:")), "")
    return {
        "doc": page["doc"],
        "url": page["url"],
        "hash": page["hash"],
        "voice": {"id": voice.name, "attribution": attribution},
        "model": who.model,
        "duration": round(sum(l["duration"] for l in lines), 3),
        "lines": lines,
    }


async def generate(a: argparse.Namespace) -> None:
    t0 = time.perf_counter()
    if not a.no_build:
        bun = shutil.which("bun") or "bun"
        await run("build", bun, "gen/build.ts", cwd=SITE, bound=300)
    pages = json.loads(SPOKEN.read_text())
    if a.pages:
        unknown = set(a.pages) - {p["url"] for p in pages}
        if unknown:
            raise StepError(f"pages: no page at {', '.join(sorted(unknown))}")
        pages = [p for p in pages if p["url"] in a.pages]
    else:
        pages = [p for p in pages if p["url"] != "/404"]
    who = identity(a.tts, a.voice)
    lex = Lexicon.load()

    def line_of(p: dict, l: dict) -> Line:
        spoken, seed = lex.say(l["text"]), seed_of(l["text"])
        return Line(p["url"], l["id"], l["block"], l["text"], spoken, seed, who.key(spoken, seed))

    lines = [line_of(p, l) for p in pages for l in p["lines"]]
    stats = Stats()
    entries: dict[str, dict] = {}
    todo: list[Line] = []
    for line in lines:
        if line.key in entries:
            continue
        if (e := cached(line.key)) is not None:
            entries[line.key] = e
            stats.cached += 1
        else:
            todo.append(line)
    # One text said on several pages is generated once.
    todo = list({l.key: l for l in todo}.values())
    words = sum(len(speakable(l.spoken).split()) for l in todo)
    print(f"{len(lines)} lines on {len(pages)} pages: {stats.cached} cached, {len(todo)} to generate ({words} words, "
          f"about {words / WORDS_PER_S / 60:.1f} min of audio)", file=sys.stderr)
    if a.dry_run:
        for l in todo:
            print(f"  {l.page} {l.id}: {l.spoken}")
        return
    if todo:
        ffmpeg = shutil.which("ffmpeg")
        if not ffmpeg:
            raise StepError("encode: ffmpeg is not on PATH (brew install ffmpeg; it needs libopus)")
        server = Server(a.tts, a.voice)
        ready = await server.start()
        print(f"tts ready: {ready}", file=sys.stderr)
        try:
            with tempfile.TemporaryDirectory(prefix="site-audio-") as tmp:
                for i, line in enumerate(todo, 1):
                    try:
                        entries[line.key] = await voice_line(server, line, ffmpeg, stats, Path(tmp))
                    except StepError as e:
                        stats.failed.append(f"{line.page} {line.id}: {e}")
                        print(f"FAILED {line.page} {line.id}: {e}", file=sys.stderr)
                        if server.proc is None or server.proc.returncode is not None:
                            raise
                        continue
                    e = entries[line.key]
                    print(f"[{i}/{len(todo)}] {line.page} {line.id}: {e['duration']} s, gen {e['gen_s']} s", file=sys.stderr)
        finally:
            await server.stop()
    written = []
    for p in pages:
        by_id = {}
        for l in p["lines"]:
            key = who.key(lex.say(l["text"]), seed_of(l["text"]))
            if key in entries:
                by_id[l["id"]] = entries[key]
        if len(by_id) != len(p["lines"]):
            print(f"{p['url']}: {len(p['lines']) - len(by_id)} lines not voiced; no manifest", file=sys.stderr)
            continue
        out = MANIFESTS / f"{p['doc']}.json"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(manifest(p, by_id, who, a.voice, lex), indent=1) + "\n")
        written.append(p["url"])
    used = {e["file"] for e in entries.values()}
    size = sum((FILES / f).stat().st_size for f in used)
    total = sum(e["duration"] for e in entries.values())
    print(json.dumps({
        "pages": written,
        "lines": len(lines),
        "generated": stats.generated,
        "cached": stats.cached,
        "retakes": stats.retakes,
        "failed": stats.failed,
        "audio_min": round(total / 60, 2),
        "generated_audio_min": round(stats.audio_s / 60, 2),
        "model_gen_s": round(stats.gen_s, 1),
        "wall_s": round(time.perf_counter() - t0, 1),
        "bytes": size,
    }, indent=1))
    if stats.failed:
        raise StepError(f"{len(stats.failed)} lines failed")


TRANSCRIBE = r"""
import json, sys
import mlx_whisper
reqs = json.load(sys.stdin)
out = []
for r in reqs:
    t = mlx_whisper.transcribe(r["path"], path_or_hf_repo=r["model"], language="en")["text"].strip()
    out.append({"id": r["id"], "asr": t})
    print(f"transcribed {r['id']}", file=sys.stderr, flush=True)
print(json.dumps(out))
"""


ONES = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"]
TENS = ["_", "_", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"]


def number(n: int) -> list[str]:
    """An integer in words, as align.py says it (digit by digit past the billions)."""
    if n < 20:
        return [ONES[n]]
    if n < 100:
        return [TENS[n // 10]] + ([] if n % 10 == 0 else [ONES[n % 10]])
    if n < 1000:
        return [ONES[n // 100], "hundred"] + ([] if n % 100 == 0 else number(n % 100))
    for size, name in ((10**9, "billion"), (10**6, "million"), (1000, "thousand")):
        if size <= n < 1000 * size:
            return number(n // size) + [name] + ([] if n % size == 0 else number(n % size))
    return [ONES[int(d)] for d in str(n)]


def words_of(text: str) -> list[str]:
    """A line as comparable words: lowercase, punctuation dropped, numbers in words ("6.33" is six point three three),
    and a run of single letters as one word ("R A jit", "blaze" and "blaze" are all `blaze`), so the transcript and
    the text compare however Whisper spells what it heard."""
    out: list[str] = []
    for w in speakable(text).replace("-", " ").replace("/", " ").split():
        for part in re.findall(r"\d+(?:\.\d+)?|[^\W\d_]+", w.lower().replace(",", "")):
            if part[0].isdigit():
                whole, _, frac = part.partition(".")
                out += number(int(whole)) + (["point", *(ONES[int(d)] for d in frac)] if frac else [])
            else:
                out.append(part)
    merged: list[str] = []
    letters = False  # merged[-1] is a run of single letters
    for w in out:
        single = len(w) == 1 and w.isalpha()
        if single and letters:
            merged[-1] += w
        else:
            merged.append(w)
        letters = single
    return merged


async def check(a: argparse.Namespace) -> None:
    """Whisper transcripts of each line of the pages' manifests against the spoken text: word error rate per line.
    Both sides go through the lexicon (the transcript in any case), so "rustc" heard as "Rust C" is no error."""
    import difflib

    lex = Lexicon.load()

    urls = set(a.pages)
    reqs, texts = [], {}
    spoken = {p["url"]: p for p in json.loads(SPOKEN.read_text())}
    for url in sorted(urls):
        p = spoken[url]
        m = json.loads((MANIFESTS / f"{p['doc']}.json").read_text())
        by_id = {l["id"]: l for l in p["lines"]}
        for l in m["lines"]:
            rid = f"{url} {l['id']}"
            reqs.append({"id": rid, "path": str(FILES / l["src"]), "model": a.whisper})
            texts[rid] = by_id[l["id"]]["text"]
    uv = shutil.which("uv") or "uv"
    out = await run(
        "whisper", uv, "run", "--directory", str(a.tts), "--group", "dev", "python", "-c", TRANSCRIBE,
        stdin=json.dumps(reqs).encode(), bound=3600,
    )
    results = json.loads(out.decode().strip().splitlines()[-1])
    total_err = total_words = 0
    rows = []
    for r in results:
        ref, hyp = words_of(lex.say(texts[r["id"]], fold=True)), words_of(lex.say(r["asr"], fold=True))
        sm = difflib.SequenceMatcher(a=ref, b=hyp, autojunk=False)
        matched = sum(b.size for b in sm.get_matching_blocks())
        errors = max(len(ref), len(hyp)) - matched
        total_err += errors
        total_words += len(ref)
        rows.append({"id": r["id"], "wer": round(errors / max(len(ref), 1), 3), "text": speakable(lex.say(texts[r["id"]])),
                     "asr": r["asr"]})
    for row in rows:
        flag = "  " if row["wer"] <= 0.15 else "!!"
        print(f"{flag} {row['wer']:.2f} {row['id']}\n     text: {row['text']}\n     asr:  {row['asr']}")
    print(json.dumps({"lines": len(rows), "words": total_words, "wer": round(total_err / max(total_words, 1), 4),
                      "lines_over_15pct": sum(r["wer"] > 0.15 for r in rows)}))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tts", type=Path, default=DEFAULT_TTS, help="the voice repo's tools/tts")
    ap.add_argument("--voice", type=Path, default=DEFAULT_VOICE, help="reference voice dir (ref.wav, ref.txt)")
    ap.add_argument("pages", nargs="*", help="page URLs (default: every page but /404)")
    ap.add_argument("--check", action="store_true", help="transcribe the pages' audio with Whisper and compare")
    ap.add_argument("--whisper", default="mlx-community/whisper-large-v3-turbo")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--no-build", action="store_true", help="use .gen/spoken.json as it is")
    a = ap.parse_args()
    if a.check and not a.pages:
        ap.error("--check needs the pages to check")
    asyncio.run(check(a) if a.check else generate(a))


if __name__ == "__main__":
    try:
        main()
    except* StepError as eg:
        for e in eg.exceptions:
            print(e, file=sys.stderr)
        sys.exit(1)
