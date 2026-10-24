#!/usr/bin/env python3
"""Regenerate the README screenshots and demo animation in .github/assets/.

    python3 scripts/screenshots/generate.py [--binary PATH] [--out DIR] [--keep-demo]

Builds the release binary, creates a synthetic home directory from APFS clones
(about 2 GiB of real disk, 50 GiB of reported content), drives diskr in a
pseudo-terminal with that HOME, and renders what the terminal shows. Run it
from a normal macOS terminal session; sandboxes that deny PTY allocation fail.

Requirements: macOS on APFS, Python 3.9+, `img2webp` (Homebrew `webp`), and the
system SF Mono/SF Pro fonts. Python packages are pinned in requirements.txt and
installed into target/screenshots/venv on first run. The disks panel shows the
host's real volumes; host paths and the user name are masked as /Users/demo,
and the run aborts before writing if any remain visible.
"""
import argparse
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
WORK = os.path.join(REPO, "target", "screenshots")
VENV = os.path.join(WORK, "venv")
SHOWN_HOME = "/Users/demo"


def ensure_venv():
    if os.environ.get("DISKR_SCREENSHOTS_VENV") == "1":
        return
    python = os.path.join(VENV, "bin", "python")
    if not os.path.exists(python):
        subprocess.run([sys.executable, "-m", "venv", VENV], check=True)
    requirements = os.path.join(HERE, "requirements.txt")
    stamp = os.path.join(VENV, "requirements.stamp")
    wanted = open(requirements).read()
    if not os.path.exists(stamp) or open(stamp).read() != wanted:
        subprocess.run([python, "-m", "pip", "install", "--quiet", "-r", requirements], check=True)
        with open(stamp, "w") as f:
            f.write(wanted)
    env = dict(os.environ, DISKR_SCREENSHOTS_VENV="1")
    os.execve(python, [python, os.path.abspath(__file__)] + sys.argv[1:], env)


def preflight(render):
    if sys.platform != "darwin":
        sys.exit("screenshots require macOS")
    if not shutil.which("img2webp"):
        sys.exit("img2webp not found; install libwebp (brew install webp)")
    missing = [f for f in render.REQUIRED_FONTS if not os.path.exists(f)]
    if missing:
        sys.exit(f"missing system fonts: {', '.join(missing)}")
    os.makedirs(WORK, exist_ok=True)
    if shutil.disk_usage(WORK).free < 4 * 1024**3:
        sys.exit("need at least 4 GiB free for the demo home")


def demo_path_dir():
    # Hide the host's system Python/pip so the packages panel only shows the
    # demo's projects, not whatever the Command Line Tools installed.
    bin_dir = os.path.join(WORK, "bin")
    shutil.rmtree(bin_dir, ignore_errors=True)
    os.makedirs(bin_dir)
    for name in os.listdir("/usr/bin"):
        if not name.startswith(("pip", "python", "pydoc", "easy_install", "2to3")):
            os.symlink(os.path.join("/usr/bin", name), os.path.join(bin_dir, name))
    return f"{bin_dir}:/bin:/usr/sbin:/sbin"


class Run:
    def __init__(self, binary, home, path):
        self.binary = binary
        self.home = home
        self.state = os.path.join(home, "Library", "Application Support", "diskr")
        self.primed = os.path.join(WORK, "primed-state")
        self.env = {
            "HOME": home, "PATH": path, "TERM": "xterm-256color", "COLORTERM": "truecolor",
            "LANG": "en_US.UTF-8", "USER": "demo", "LOGNAME": "demo",
        }

    def session(self, cols, rows):
        from terminal import Session
        return Session([self.binary], self.env, cols, rows)

    def save_primed_state(self):
        shutil.rmtree(self.primed, ignore_errors=True)
        shutil.copytree(self.state, self.primed)

    def restore_primed_state(self):
        # Every capture starts from the first session's size cache so changed
        # directories show growth badges.
        shutil.rmtree(self.state, ignore_errors=True)
        shutil.copytree(self.primed, self.state)


def scan_all(s):
    """Size every visible directory; retry once if S landed during a reload."""
    s.wait(1.5)
    for attempt in range(2):
        s.key("S")
        try:
            s.until("full scan complete", timeout=20)
            return
        except TimeoutError:
            if attempt:
                raise


def prime(run):
    with run.session(120, 22) as s:
        s.ensure("hidden on", ".")
        scan_all(s)
        s.wait(2)
    run.save_primed_state()


def record_walkthrough(run):
    run.restore_primed_state()
    with run.session(120, 22) as s:
        s.record()
        s.wait(1.2)
        s.ensure("hidden on", ".")
        s.wait(0.8)
        scan_all(s)
        s.wait(1.8)
        s.key("home")
        s.key("down", pause=0.7)  # code
        s.key("enter", pause=1.4)
        s.key("enter", pause=1.8)  # largest project
        s.key("backspace", pause=0.8)
        s.key("backspace", pause=1.2)
        for _ in range(3):
            s.key("tab")
        s.until("candidate disk bytes")
        s.wait(1.6)
        for _ in range(7):
            s.key("down", pause=0.6)
        s.wait(1.2)
        s.key("esc", pause=1.0)
        s.key("home", pause=0.6)
        s.key("t")
        s.until("top files scan complete")
        s.wait(2.2)
        s.key("esc", pause=1.5)
        return s.stop_recording()


def capture_files_and_top(run):
    run.restore_primed_state()
    with run.session(120, 22) as s:
        s.ensure("hidden on", ".")
        scan_all(s)
        s.wait(1)
        s.key("p")
        s.until("dependency leaves")
        s.key("p")
        s.until(" deps · ")
        s.wait(1)
        s.key("esc", pause=1.0)
        s.key("home")
        for _ in range(4):  # cycle sort modes back to size; leaves a sort status line
            s.key("o", pause=0.5)
        s.until("sort: size")
        s.wait(1)
        files = s.grid()
        s.key("t")
        s.until("top files scan complete")
        s.wait(1)
        return files, s.grid()


def capture_reclaim(run):
    with run.session(120, 28) as s:
        s.ensure("hidden on", ".")
        s.wait(1)
        for _ in range(3):
            s.key("tab")
        s.until("candidate disk bytes")
        s.wait(1)
        return s.grid()


def main():
    ensure_venv()
    sys.dont_write_bytecode = True
    sys.path.insert(0, HERE)
    import demo_home
    import render

    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--binary", help="diskr binary to capture (default: build target/release/diskr)")
    ap.add_argument("--out", default=os.path.join(REPO, ".github", "assets"))
    ap.add_argument("--keep-demo", action="store_true", help="keep the demo home after rendering")
    args = ap.parse_args()

    preflight(render)
    binary = args.binary
    if not binary:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=REPO, check=True)
        binary = os.path.join(REPO, "target", "release", "diskr")
    binary = os.path.realpath(binary)

    demo_root = os.path.join(WORK, "demo")
    os.makedirs(demo_root, exist_ok=True)
    home = os.path.realpath(os.path.join(demo_root, "home"))
    base = os.path.join(demo_root, "base.bin")
    masker = render.Masker(
        paths=[home, os.path.realpath(os.path.expanduser("~")), os.path.realpath(WORK)],
        shown=SHOWN_HOME, user=os.environ.get("USER", ""))

    print("building demo home")
    demo_home.build(home, base)
    run = Run(binary, home, demo_path_dir())
    try:
        print("priming size cache")
        prime(run)
        demo_home.grow(home, base)
        print("recording walkthrough")
        frames = record_walkthrough(run)
        print("capturing stills")
        files, top = capture_files_and_top(run)
        reclaim = capture_reclaim(run)
    finally:
        if not args.keep_demo:
            shutil.rmtree(demo_root, ignore_errors=True)

    stills = {"files.png": (files, 22), "top-files.png": (top, 22), "reclaim.png": (reclaim, 28)}
    masked = {name: masker.apply(cells) for name, (cells, _) in stills.items()}
    for frame in frames:
        frame["cells"] = masker.apply(frame["cells"])
    leaks = [line for cells in list(masked.values()) + [f["cells"] for f in frames]
             for line in masker.leaks(cells)]
    if leaks:
        sys.exit("host paths still visible after masking:\n" + "\n".join(sorted(set(leaks))))

    os.makedirs(args.out, exist_ok=True)
    for name, (_, rows) in stills.items():
        render.terminal_image(masked[name], 120, rows, 2).save(os.path.join(args.out, name), optimize=True)
    render.animate(frames, 120, 22, 2, os.path.join(args.out, "demo.webp"), os.path.join(WORK, "frames"))
    render.social_card(os.path.join(args.out, "files.png"), os.path.join(args.out, "social-preview.png"))
    shutil.rmtree(os.path.join(WORK, "frames"), ignore_errors=True)
    for name in ["demo.webp", "files.png", "reclaim.png", "top-files.png", "social-preview.png"]:
        path = os.path.join(args.out, name)
        print(f"{os.path.getsize(path) // 1024:6d} KiB  {os.path.relpath(path, REPO)}")


if __name__ == "__main__":
    main()
