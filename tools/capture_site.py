import codecs, fcntl, hashlib, json, os, pty, select, shutil, struct, sys, tempfile, time, termios
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont
import pyte

ROOT = Path.cwd()
OUT = ROOT / "docs/assets/captures"
font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 18)
rows = [
    "    1201  37.5 execute",
    "    1202  22.8 rustc",
    "    1203  12.4 gh",
    "    1204   8.9 jq",
    "    1205   6.2 tail",
    "    1206   4.0 cat",
    "    1207   3.1 sort",
    "    1208   2.7 grep",
    "    1209   1.8 git",
    "    1210   0.6 printf",
]
metadata = []
for name, example in [("cpu", "top-cpu.program"), ("stars", "github-stars.program")]:
    with tempfile.TemporaryDirectory(prefix="execute-site-capture-") as directory:
        folder = Path(directory)
        catalog = folder / "manifests"
        shutil.copytree(ROOT / "manifests", catalog)
        fixture = folder / "fixture.py"
        fixture.write_text(
            "import json,sys,pathlib\n"
            + (
                "print(" + repr("\n".join(rows)) + ")\n"
                if name == "cpu"
                else 'p=pathlib.Path(__file__).with_suffix(".count"); n=int(p.read_text())+1 if p.exists() else 0;p.write_text(str(n));print(json.dumps({"nameWithOwner":"morluto/rea","stargazerCount":1200+n*3,"forkCount":84,"description":"Fixture data. Real Execute terminal renderer."}))\n'
            )
        )
        manifest = catalog / ("ps.json" if name == "cpu" else "gh.repo.json")
        m = json.loads(manifest.read_text())
        m["binary"] = sys.executable
        m["arguments"] = [{"literal": "-I"}, {"literal": str(fixture)}] + (
            [{"when_present": "flags", "append": [{"spread": "flags"}]}]
            if name == "cpu"
            else [{"parameter": "repository"}]
        )
        manifest.write_text(json.dumps(m))
        pid, fd = pty.fork()
        if pid == 0:
            os.environ["TERM"] = "xterm-256color"
            os.environ["PATH"] = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin"
            os.execv(
                str(ROOT / "target/release/execute"),
                [
                    str(ROOT / "target/release/execute"),
                    str(ROOT / "examples" / example),
                    "--manifest-dir",
                    str(catalog),
                ],
            )
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 140, 0, 0))
        screen = pyte.Screen(140, 32)
        stream = pyte.Stream(screen)
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        started = time.monotonic()
        next_refresh = started + 1
        raw = bytearray()
        try:
            while time.monotonic() - started < (1.4 if name == "cpu" else 5.5):
                if select.select([fd], [], [], 0.1)[0]:
                    data = os.read(fd, 65536)
                    raw.extend(data)
                    stream.feed(decoder.decode(data))
                if name == "stars" and time.monotonic() >= next_refresh:
                    os.write(fd, b"\x12")
                    next_refresh += 1
            lines = screen.display
            if not any(
                ("1201" if name == "cpu" else "Fixture data") in line for line in lines
            ):
                raise RuntimeError(
                    "UI did not reach live fixture state: " + repr(lines)
                )
            OUT.joinpath(name + ".txt").write_text(
                "\n".join(line.rstrip() for line in lines) + "\n"
            )
            OUT.joinpath(name + ".ansi").write_bytes(raw)
            w = round(font.getlength("M"))
            h = 25
            image = Image.new("RGB", (140 * w + 48, 32 * h + 78), "#101b29")
            draw = ImageDraw.Draw(image)
            draw.text(
                (25, 16),
                "●  ●  ●   execute " + example + "   ·   fixture capture",
                font=font,
                fill="#afbdce",
            )
            for y, line in enumerate(lines):
                draw.text(
                    (24, 62 + y * h),
                    "".join(" " if 0x2800 <= ord(c) <= 0x28FF else c for c in line),
                    font=font,
                    fill="#d5e5ef",
                )
                for x, char in enumerate(line):
                    if 0x2800 <= ord(char) <= 0x28FF:
                        for bit, (dx, dy) in enumerate(
                            [
                                (0, 0),
                                (0, 1),
                                (0, 2),
                                (1, 0),
                                (1, 1),
                                (1, 2),
                                (0, 3),
                                (1, 3),
                            ]
                        ):
                            if (ord(char) - 0x2800) & (1 << bit):
                                cx = 24 + x * w + 3 + dx * 5
                                cy = 62 + y * h + 5 + dy * 4.5
                                draw.ellipse(
                                    (cx - 1, cy - 1, cx + 1, cy + 1), fill="#d5e5ef"
                                )
            image.save(OUT / (name + ".png"))
            metadata.append(
                {
                    "image": name + ".png",
                    "program": "examples/" + example,
                    "program_sha256": hashlib.sha256(
                        (ROOT / "examples" / example).read_bytes()
                    ).hexdigest(),
                    "runtime_sha256": hashlib.sha256(
                        (ROOT / "target/release/execute").read_bytes()
                    ).hexdigest(),
                    "method": "Real runtime PTY frame, 140x32 cells. pyte interprets ANSI; Pillow renders the captured cell grid in Menlo. Monochrome terminal theme. Native ps/gh manifests replaced only for this capture with deterministic private fixture backends. Application source unchanged; all layout/events/control flow remain .program. Metrics are illustrative fixture values, not live measurements.",
                }
            )
        finally:
            os.write(fd, b"\x11")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                waited, status = os.waitpid(pid, os.WNOHANG)
                if waited:
                    break
                if select.select([fd], [], [], 0.05)[0]:
                    try:
                        os.read(fd, 65536)
                    except OSError:
                        pass
            else:
                os.kill(pid, 15)
                os.waitpid(pid, 0)
            os.close(fd)
OUT.joinpath("provenance.json").write_text(json.dumps(metadata, indent=2) + "\n")
print("Captured two real TUI frames with explicit fixture data.")
