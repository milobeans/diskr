#!/usr/bin/env python3
"""Regenerate the README screenshots, demo animations and docs-site replay.

    python3 scripts/screenshots/generate.py [--binary PATH] [--out DIR] [--keep-demo]

Builds the release binary, creates a synthetic home directory and Homebrew
Cellar from APFS clones (about 2 GiB of real disk, 60 GiB of reported content),
drives diskr in a pseudo-terminal with that HOME, and renders what the terminal
shows. Run it from a normal macOS terminal session; sandboxes that deny PTY
allocation fail.

The demo lives on a throwaway APFS disk image. The cleanup walkthrough moves a
directory to the Trash, and macOS sends Trash moves to the volume's own
.Trashes rather than to $HOME, so this keeps them off the real user Trash;
detaching and deleting the image discards them. `brew` is a shim on the demo
PATH that serves the cloned Cellar and deletes kegs on uninstall.

Requirements: macOS, Python 3.9+, `img2webp` (Homebrew `webp`), `hdiutil`, and
the system SF Mono/SF Pro fonts. Python packages are pinned in requirements.txt
and installed into target/screenshots/venv on first run. The disks panel shows
the host's real volumes; host paths and the user name are masked, and the run
aborts before writing if any remain visible.
"""
import argparse
import os
import plistlib
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
WORK = os.path.join(REPO, "target", "screenshots")
VENV = os.path.join(WORK, "venv")
IMAGE = os.path.join(WORK, "demo.sparseimage")
MOUNT = os.path.join(WORK, "Demo")  # the disks panel labels volumes by mount folder
SHOWN_HOME = "/Users/demo"
COLS, ROWS = 120, 22

BREW_SHIM = """#!/bin/sh
exec __PYTHON__ - "$@" <<'DISKR_DEMO_PY'
# Demo stand-in for Homebrew: serves the cloned Cellar for README captures.
import os
import shutil
import sys
from pathlib import Path

prefix = Path(__PREFIX__)
allowed = __PACKAGES__
args = sys.argv[1:]
command = args[0] if args else ""
if command in ("--prefix", "--repository"):
    print(prefix)
elif command == "--cellar":
    print(prefix / "Cellar")
elif command == "--caskroom":
    print(prefix / "Caskroom")
elif command == "--version":
    print("Homebrew 4.6.0")
elif command == "info":
    print('{"formulae":[],"casks":[]}')
elif command == "deps":
    lines = (prefix / "deps.txt").read_text().splitlines()
    for keg in sorted((prefix / "Cellar").iterdir()):
        print(next((line for line in lines if line.startswith(keg.name + ":")), keg.name + ":"))
elif command == "uninstall":
    if len(args) != 2 or args[1] not in allowed:
        sys.exit("demo brew: refusing unexpected uninstall target")
    cellar = (prefix / "Cellar").resolve(strict=True)
    target = cellar / args[1]
    if target.is_symlink() or target.resolve(strict=True).parent != cellar:
        sys.exit("demo brew: target escaped the synthetic Cellar")
    if os.stat(target).st_dev != os.stat(prefix).st_dev:
        sys.exit("demo brew: target is on a different volume")
    print(f"Uninstalling {target}...")
    shutil.rmtree(target)
else:
    sys.exit("demo brew: unsupported command: " + " ".join(args))
DISKR_DEMO_PY
"""


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
    for tool, hint in [("img2webp", "install libwebp (brew install webp)"), ("hdiutil", "part of macOS")]:
        if not shutil.which(tool):
            sys.exit(f"{tool} not found; {hint}")
    missing = [f for f in render.REQUIRED_FONTS if not os.path.exists(f)]
    if missing:
        sys.exit(f"missing system fonts: {', '.join(missing)}")
    os.makedirs(WORK, exist_ok=True)
    if shutil.disk_usage(WORK).free < 4 * 1024**3:
        sys.exit("need at least 4 GiB free for the demo volume")


def attach_volume():
    detach_volume()
    if os.path.exists(IMAGE):
        os.remove(IMAGE)
    subprocess.run(["hdiutil", "create", "-quiet", "-size", "12g", "-fs", "APFS",
                    "-volname", "Demo", "-type", "SPARSE", IMAGE], check=True)
    os.makedirs(MOUNT, exist_ok=True)
    subprocess.run(["hdiutil", "attach", "-quiet", "-nobrowse", "-noautoopen",
                    "-mountpoint", MOUNT, IMAGE], check=True)
    verify_demo_volume()
    trash = os.path.join(MOUNT, ".Trashes")
    os.makedirs(trash, mode=0o1777, exist_ok=True)
    os.chmod(trash, 0o1777)
    os.makedirs(os.path.join(trash, str(os.getuid())), mode=0o700, exist_ok=True)


def owned_image_mount(info, image_path=IMAGE, mount_path=MOUNT):
    """Return the mounted device only when hdiutil ties it to our image."""
    image_path = os.path.realpath(image_path)
    mount_path = os.path.realpath(mount_path)
    for image in info.get("images", []):
        if os.path.realpath(image.get("image-path", "")) != image_path:
            continue
        for entity in image.get("system-entities", []):
            if entity.get("mount-point") and os.path.realpath(entity["mount-point"]) == mount_path:
                device = entity.get("dev-entry", "")
                if device.startswith("/dev/disk"):
                    return device
    raise RuntimeError("demo mount is not backed by the expected disposable disk image")


def verify_demo_volume():
    info = plistlib.loads(subprocess.check_output(["hdiutil", "info", "-plist"]))
    device = owned_image_mount(info)
    if not os.path.ismount(MOUNT) or os.stat(MOUNT).st_dev == os.stat(WORK).st_dev:
        raise RuntimeError("demo mount must be a separate mounted filesystem")
    return device


def verify_demo_target(path):
    verify_demo_volume()
    target = Path(path)
    mount = Path(MOUNT).resolve(strict=True)
    resolved = target.resolve(strict=True)
    if target.is_symlink() or mount not in resolved.parents:
        raise RuntimeError("refusing a demo action outside the disposable volume")
    if target.stat().st_dev != mount.stat().st_dev:
        raise RuntimeError("demo action target is on an unexpected filesystem")


def detach_volume():
    if os.path.ismount(MOUNT):
        device = verify_demo_volume()
        subprocess.run(["hdiutil", "detach", "-quiet", device], check=True)


def brew_shim(brew_prefix):
    import demo_home
    return (BREW_SHIM.replace("__PYTHON__", shlex.quote(sys.executable))
            .replace("__PREFIX__", repr(str(brew_prefix)))
            .replace("__PACKAGES__", repr([formula[0] for formula in demo_home.FORMULAE])))


def demo_path_dir(brew_prefix):
    # Hide the host's system Python/pip and real package managers so the
    # packages panel only shows the demo Cellar and projects.
    bin_dir = os.path.join(WORK, "bin")
    shutil.rmtree(bin_dir, ignore_errors=True)
    os.makedirs(bin_dir)
    for name in os.listdir("/usr/bin"):
        if not name.startswith(("pip", "python", "pydoc", "easy_install", "2to3")):
            os.symlink(os.path.join("/usr/bin", name), os.path.join(bin_dir, name))
    brew = os.path.join(bin_dir, "brew")
    with open(brew, "w") as f:
        f.write(brew_shim(brew_prefix))
    os.chmod(brew, 0o755)
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

    def session(self, rows=ROWS):
        from terminal import Session
        return Session([self.binary, self.home], self.env, COLS, rows)

    def save_primed_state(self):
        shutil.rmtree(self.primed, ignore_errors=True)
        shutil.copytree(self.state, self.primed)

    def restore_primed_state(self):
        # Start from the first session's size cache so the directories changed
        # since then show growth badges.
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
    with run.session() as s:
        s.ensure("hidden on", ".")
        scan_all(s)
        s.wait(2)
    run.save_primed_state()


def capture_stills(run):
    """Non-destructive views for the README and the site's annotated figures."""
    run.restore_primed_state()
    stills = {}
    with run.session() as s:
        s.ensure("hidden on", ".")
        scan_all(s)
        s.wait(1)
        s.key("p")
        s.until("dependency leaves")
        s.ensure("[ Projects ]", "p")
        s.until(" deps · ")
        s.wait(1)
        s.key("esc", pause=1.0)
        s.key("home")
        for _ in range(4):  # cycle sort modes back to size; leaves a sort status line
            s.key("o", pause=0.5)
        s.until("sort: size")
        s.wait(1)
        stills["files"] = s.grid()
        s.key("t")
        s.until("top files scan complete")
        s.wait(1)
        stills["top"] = s.grid()
        s.key("esc", pause=1.0)
        s.key("home")
        s.key("down", pause=0.5)  # code
        s.key("enter", pause=1.0)
        s.key("enter", pause=1.0)  # largest project
        s.until("[build]")
        s.key("home", pause=1.0)
        stills["project"] = s.grid()
        s.key("backspace", pause=0.8)
        s.key("backspace", pause=0.8)
        for _ in range(3):
            s.key("tab")
        s.until("candidate disk bytes")
        s.wait(1)
        stills["reclaim"] = s.grid()
    with run.session(rows=28) as s:
        s.wait(1)
        s.ensure("hidden on", ".")
        s.wait(1)
        for _ in range(3):
            s.key("tab")
        s.until("candidate disk bytes")
        s.wait(1)
        stills["reclaim-tall"] = s.grid()
    return stills


def confirmation_matches(text, title, subject, path_tail):
    """Match adjacent modal lines, not a package/path visible behind it."""
    lines = text.splitlines()
    for index, line in enumerate(lines[:-2]):
        column = line.find(title)
        if column < 0:
            continue
        body = [row[column:].split("│", 1)[0].strip() for row in lines[index + 1:index + 3]]
        if body[0] == subject and body[1].endswith(path_tail):
            return True
    return False


def record_cleanup_walkthrough(run):
    """Follow the sizes down from ~ to the biggest build output and trash it."""
    target = os.path.join(run.home, "code", "ledger-api", "target")
    verify_demo_target(target)
    with run.session() as s:
        s.record()
        s.wait(2.0)  # home folder, largest first
        s.key("home", pause=0.8)
        s.key("down", pause=1.1)  # code
        s.key("enter", pause=1.0)
        s.key("home", pause=1.2)  # ledger-api, the largest project
        s.key("enter")
        s.until("[build]")
        s.wait(0.6)
        s.key("home", pause=1.5)  # target: build output
        s.key("d")
        s.until_screen(lambda text: confirmation_matches(
            text, "Move to Trash:", "target", "ledger-api/target"), "synthetic build confirmation")
        s.wait(1.8)
        verify_demo_target(target)
        s.key("y")
        s.until("moved to trash: target")
        if os.path.lexists(target):
            raise RuntimeError("cleanup demo did not remove the intended synthetic build directory")
        trash = Path(MOUNT) / ".Trashes" / str(os.getuid())
        if not any(p.name.startswith("target") for p in trash.iterdir()):
            raise RuntimeError("cleanup demo did not retain its build directory in the demo volume Trash")
        s.wait(1.8)
        s.key("backspace", pause=1.6)
        s.key("backspace", pause=2.6)
        return s.stop_recording()


def record_packages_walkthrough(run):
    """Open packages, keep only dependency leaves, uninstall the biggest one."""
    import demo_home

    target = os.path.join(MOUNT, "opt", "homebrew", "Cellar", "llvm@18")
    verify_demo_target(target)
    with run.session() as s:
        s.record()
        s.wait(1.6)
        s.key("p")
        s.until("dependency leaves")
        s.ensure("[ System ]", "p")
        s.until("llvm@18")
        s.wait(2.0)
        s.key("u", pause=2.0)  # dependency leaves only
        s.key("home", pause=1.0)  # llvm@18, an old toolchain nothing depends on
        s.key("x")
        s.until_screen(lambda text: confirmation_matches(
            text, "Uninstall package:", "brew llvm@18", "opt/homebrew"), "synthetic package confirmation")
        s.wait(1.8)
        verify_demo_target(target)
        s.key("y")
        # The success status is immediately replaced by inventory progress.
        # Require the completed, smaller inventory and the intended removal.
        remaining = len(demo_home.FORMULAE) - 1
        s.until_screen(lambda text: f"{remaining} packages · " in text and not os.path.lexists(target),
                       "refreshed inventory after the synthetic package removal")
        if os.path.lexists(target):
            raise RuntimeError("package demo did not remove the intended synthetic package")
        s.wait(2.6)
        return s.stop_recording()


def main():
    ensure_venv()
    sys.dont_write_bytecode = True
    sys.path.insert(0, HERE)
    import demo_home
    import render

    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--binary", help="diskr binary to capture (default: build target/release/diskr)")
    ap.add_argument("--out", default=os.path.join(REPO, ".github", "assets"))
    ap.add_argument("--site-out", default=os.path.join(REPO, "site", "assets", "replay.json"),
                    help="replay data for the docs site")
    ap.add_argument("--keep-demo", action="store_true", help="leave the demo volume mounted")
    args = ap.parse_args()

    preflight(render)
    binary = args.binary
    if not binary:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=REPO, check=True)
        binary = os.path.join(REPO, "target", "release", "diskr")
    binary = os.path.realpath(binary)

    try:
        attach_volume()
        mount = os.path.realpath(MOUNT)
        home = os.path.join(mount, "home")
        brew_prefix = os.path.join(mount, "opt", "homebrew")
        base = os.path.join(mount, "base.bin")
        masker = render.Masker(
            pairs=[(home, SHOWN_HOME), (brew_prefix, "/opt/homebrew"), (mount, "/Volumes/Demo"),
                   (os.path.realpath(os.path.expanduser("~")), SHOWN_HOME), (os.path.realpath(WORK), SHOWN_HOME)],
            user=os.environ.get("USER", ""))
        print("building demo home")
        demo_home.build(home, base)
        demo_home.build_homebrew(brew_prefix, base)
        run = Run(binary, home, demo_path_dir(brew_prefix))
        print("priming size cache")
        prime(run)
        demo_home.grow(home, base)
        print("capturing stills")
        stills = capture_stills(run)
        print("recording cleanup walkthrough")
        cleanup = record_cleanup_walkthrough(run)
        print("recording packages walkthrough")
        packages = record_packages_walkthrough(run)
    finally:
        if not args.keep_demo:
            detach_volume()
            if os.path.exists(IMAGE):
                os.remove(IMAGE)

    stills = {name: masker.apply(cells) for name, cells in stills.items()}
    for frame in cleanup + packages:
        frame["cells"] = masker.apply(frame["cells"])
    leaks = [line for cells in list(stills.values()) + [f["cells"] for f in cleanup + packages]
             for line in masker.leaks(cells)]
    if leaks:
        sys.exit("host paths still visible after masking:\n" + "\n".join(sorted(set(leaks))))

    os.makedirs(args.out, exist_ok=True)
    pngs = {"files.png": ("files", ROWS), "top-files.png": ("top", ROWS), "reclaim.png": ("reclaim-tall", 28)}
    for name, (key, rows) in pngs.items():
        render.terminal_image(stills[key], COLS, rows, 2).save(os.path.join(args.out, name), optimize=True)
    frames_dir = os.path.join(WORK, "frames")
    render.animate(cleanup, COLS, ROWS, 2, os.path.join(args.out, "demo.webp"), frames_dir)
    shutil.rmtree(frames_dir, ignore_errors=True)
    render.animate(packages, COLS, ROWS, 2, os.path.join(args.out, "packages.webp"), frames_dir)
    shutil.rmtree(frames_dir, ignore_errors=True)
    render.social_card(os.path.join(args.out, "files.png"), os.path.join(args.out, "social-preview.png"))

    played = cleanup + packages
    texts = ["\n".join("".join(c[0] or " " for c in row) for row in f["cells"]) for f in played]

    def first(needle, start=0):
        return next(i for i in range(start, len(texts)) if needle in texts[i])

    chapters = [("Find", 0), ("Trash", first("Move to Trash")), ("Packages", len(cleanup)),
                ("Uninstall", first("Uninstall package", len(cleanup)))]
    render.replay_json(played, [(k, stills[k]) for k in ["files", "project", "reclaim", "top"]],
                       chapters, COLS, ROWS, args.site_out)
    names = ["demo.webp", "packages.webp", "files.png", "reclaim.png", "top-files.png", "social-preview.png"]
    for path in [os.path.join(args.out, n) for n in names] + [args.site_out]:
        print(f"{os.path.getsize(path) // 1024:6d} KiB  {os.path.relpath(path, REPO)}")


if __name__ == "__main__":
    main()
