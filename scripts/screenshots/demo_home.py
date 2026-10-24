"""Build the synthetic home directory shown in the README screenshots.

Large files are APFS clones of one base file truncated to the wanted size, so
the tree reports realistic allocated sizes while using roughly 2 GiB of disk.
"""
import os
import random
import shutil
import subprocess
import time

KiB = 1024
MiB = 1024 * KiB
GiB = 1024 * MiB
BASE_SIZE = 1536 * MiB


class Builder:
    def __init__(self, root, base):
        self.root = root
        self.base = base
        self.now = time.time()
        self.rng = random.Random(7)

    def path(self, rel):
        path = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        return path

    def touch(self, path, age_days):
        if age_days is None:
            age_days = self.rng.uniform(1, 120)
        t = self.now - age_days * 86400
        os.utime(path, (t, t))

    def big(self, rel, size, age_days=None, apparent=None):
        path = self.path(rel)
        result = subprocess.run(["cp", "-c", self.base, path], capture_output=True, text=True)
        if result.returncode != 0:
            raise SystemExit(f"APFS clone failed ({result.stderr.strip()}); "
                             "the work directory must be on an APFS volume")
        os.truncate(path, size)
        if apparent:
            # Extending a clone leaves a sparse tail: small allocated, large apparent.
            os.truncate(path, apparent)
        self.touch(path, age_days)

    def small(self, rel, size=4 * KiB, text=None, age_days=None):
        path = self.path(rel)
        if text is not None:
            with open(path, "w") as f:
                f.write(text)
        else:
            with open(path, "wb") as f:
                f.write(os.urandom(size))
        self.touch(path, age_days)

    def many(self, rel_dir, count, lo=2 * KiB, hi=96 * KiB, ext=".js"):
        for i in range(count):
            size = self.rng.randint(lo, hi)
            rel = f"{rel_dir}/f{i:04d}{ext}"
            if size >= MiB:
                self.big(rel, size)
            else:
                self.small(rel, size)


def ensure_base(base):
    if os.path.exists(base) and os.path.getsize(base) == BASE_SIZE:
        return
    chunk = os.urandom(MiB)
    with open(base, "wb") as f:
        for _ in range(BASE_SIZE // MiB):
            f.write(chunk)


def build(root, base):
    ensure_base(base)
    if os.path.exists(root):
        shutil.rmtree(root)
    os.makedirs(root)
    b = Builder(root, base)

    # Library: caches, Xcode, simulators, Docker
    b.big("Library/Caches/Homebrew/downloads/llvm--21.1.4.arm64_tahoe.bottle.tar.gz", 1180 * MiB, 9)
    b.big("Library/Caches/Homebrew/downloads/qt--6.9.3.arm64_tahoe.bottle.tar.gz", 610 * MiB, 21)
    b.big("Library/Caches/com.spotify.client/Data/storage.bnk", 940 * MiB, 2)
    b.big("Library/Caches/Google/Chrome/Default/Cache/Cache_Data/data_3", 655 * MiB, 1)
    b.big("Library/Caches/pip/http-v2/wheels.cache", 430 * MiB, 33)
    b.many("Library/Caches/com.apple.Safari/WebKitCache", 60, ext=".blob")
    for i, name in enumerate(["Orbit-dkqzbhtvmkx", "Ledger-fhwqmbcnaa", "Pulse-ezxoqkdhrr"]):
        for j in range(4 - i):
            b.big(f"Library/Developer/Xcode/DerivedData/{name}/Build/Intermediates.noindex/obj{j}.o",
                  b.rng.randint(700, 1400) * MiB, 3 + i * 5)
    b.big("Library/Developer/Xcode/Archives/2026-09-30/Orbit 2.4.xcarchive/Products/Orbit.app.bin",
          1240 * MiB, 24)
    b.big("Library/Developer/Xcode/iOS DeviceSupport/iPhone17,1 26.0 (23A341)/Symbols/dyld_shared_cache",
          1500 * MiB, 40)
    b.big("Library/Developer/Xcode/iOS DeviceSupport/iPhone16,2 18.6 (22G86)/Symbols/dyld_shared_cache",
          1450 * MiB, 95)
    for i in range(3):
        b.big(f"Library/Developer/CoreSimulator/Devices/5F1C0A{i}E-2B7D/data/Containers/data.img",
              b.rng.randint(1100, 1500) * MiB, 6 + i)
    b.big("Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw", 1500 * MiB, 0.2,
          apparent=64 * GiB)
    b.small("Library/Preferences/com.apple.finder.plist", 12 * KiB)

    # Dotfile caches, toolchains and models
    b.big(".cargo/registry/cache/index.crates.io-1949cf8c6b5b557f/crates.blob", 1320 * MiB, 4)
    b.many(".cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228/src", 40, ext=".rs")
    b.big(".npm/_cacache/content-v2/sha512/blob", 780 * MiB, 12)
    b.big(".Trash/Install macOS Tahoe.dmg", 1490 * MiB, 3)
    b.big(".Trash/old-vm-backup.tar", 960 * MiB, 3)
    b.big(".ollama/models/blobs/sha256-6a0746a1ec1aef3e7ec53868f220ff6e389f6f8ef87a01d77c96807de94ca2aa",
          1480 * MiB, 20)
    b.big(".ollama/models/blobs/sha256-a3de86cd1c132c822487ededd47a324c50491393e6565cd14bafa40d0b8e686f",
          1390 * MiB, 44)
    b.small(".ollama/models/manifests/registry.ollama.ai/library/llama3.2/latest", text="{}")
    b.big(".rustup/toolchains/stable-aarch64-apple-darwin/lib/librustc_driver.dylib", 1150 * MiB, 15)
    b.many(".rustup/toolchains/stable-aarch64-apple-darwin/share/doc/rust/html", 50,
           lo=8 * KiB, hi=200 * KiB, ext=".html")
    b.many(".vscode/extensions/rust-lang.rust-analyzer-0.3.2593/server", 6,
           lo=20 * MiB, hi=60 * MiB, ext=".bin")
    b.many(".vscode/extensions/ms-python.python-2026.18.0/out", 120, lo=10 * KiB, hi=400 * KiB)

    # Projects with build artifacts; markers make the reclaim pass trust them.
    b.small("code/orbit-web/package.json", text=(
        '{"name":"orbit-web","dependencies":{"next":"16.0.0","react":"19.2.0","react-dom":"19.2.0",'
        '"zod":"4.1.0","@tanstack/react-query":"5.90.0","tailwindcss":"4.1.0"},'
        '"devDependencies":{"typescript":"5.9.0","eslint":"9.37.0","vitest":"3.2.0","@types/react":"19.2.0"}}'))
    b.small("code/orbit-web/src/app/page.tsx", text="export default function Page() { return null }\n")
    b.big("code/orbit-web/node_modules/@next/swc-darwin-arm64/next-swc.darwin-arm64.node", 1120 * MiB, 14)
    b.many("code/orbit-web/node_modules/react-dom/cjs", 80)
    b.big("code/orbit-web/.next/cache/webpack/client-production/0.pack", 520 * MiB, 1)
    b.small("code/ledger-api/Cargo.toml", text=(
        '[package]\nname = "ledger-api"\nversion = "0.4.2"\n\n[dependencies]\naxum = "0.8"\n'
        'tokio = { version = "1", features = ["full"] }\nserde = { version = "1", features = ["derive"] }\n'
        'serde_json = "1"\nsqlx = "0.8"\ntracing = "0.1"\nanyhow = "1"\n\n[dev-dependencies]\ninsta = "1"\n'))
    b.small("code/ledger-api/src/main.rs", text="fn main() {}\n")
    b.big("code/ledger-api/target/debug/deps/libledger_api.rlib", 1380 * MiB, 0.5)
    b.big("code/ledger-api/target/release/deps/libledger_api.rlib", 1290 * MiB, 6)
    b.big("code/ledger-api/target/debug/incremental/ledger_api-1x2y/s-h8.bin", 870 * MiB, 0.5)
    b.small("code/pulse-ml/pyproject.toml", text=(
        '[project]\nname = "pulse-ml"\ndependencies = ["torch", "numpy", "pandas", "polars", '
        '"scikit-learn", "matplotlib", "jupyterlab"]\n'))
    b.small("code/pulse-ml/.venv/pyvenv.cfg", text="home = /opt/homebrew/bin\nversion = 3.13.7\n")
    b.big("code/pulse-ml/.venv/lib/python3.13/site-packages/torch/lib/libtorch_cpu.dylib", 1460 * MiB, 30)
    b.many("code/pulse-ml/src/pulse/__pycache__", 24, ext=".pyc")
    b.small("code/pulse-ml/src/pulse/train.py", text="import torch\n")
    b.big("code/pulse-ml/data/events-2026-q3.parquet", 1340 * MiB, 18)

    # Personal files
    b.big("Downloads/Screen Recording 2026-10-12 at 14.03.mov", 1410 * MiB, 12)
    b.big("Downloads/ubuntu-26.04-live-server-arm64.iso", 1500 * MiB, 26)
    b.big("Downloads/Figma-handoff-export.zip", 380 * MiB, 7)
    b.many("Downloads/receipts", 30, lo=40 * KiB, hi=400 * KiB, ext=".pdf")
    b.big("Movies/Final Cut/Orbit launch film.fcpbundle/Media.mov", 1490 * MiB, 50)
    b.big("Movies/Final Cut/Orbit launch film.fcpbundle/Render Files.mov", 1210 * MiB, 50)
    b.many("Pictures/Photos Library.photoslibrary/originals", 120, lo=1500 * KiB, hi=6000 * KiB, ext=".heic")
    b.many("Documents/Taxes 2025", 12, lo=80 * KiB, hi=900 * KiB, ext=".pdf")
    b.many("Documents/Notes", 40, lo=1 * KiB, hi=20 * KiB, ext=".md")
    b.many("Music/Logic", 20, lo=2 * MiB, hi=12 * MiB, ext=".wav")
    b.small("Desktop/todo.txt", text="ship the readme\n", age_days=0.1)


def grow(root, base):
    """Changes between two sessions: a new model, a fresh build, a download."""
    b = Builder(root, base)
    b.big(".ollama/models/blobs/sha256-dde5aa3fc5ffc17176b5e8bdc82f587b24b2678c6c66101bf7da77af9f7ccdff",
          1500 * MiB, 0)
    b.big(".ollama/models/blobs/sha256-34bb5ab01051a11372a91f95f3fbbc51173eed8e7f13ec395b9ae9b8bd0e242b",
          1270 * MiB, 0)
    b.big("code/ledger-api/target/debug/deps/libsqlx_core.rlib", 960 * MiB, 0)
    b.big("Downloads/Xcode_26.1_Release_Candidate.xip", 1500 * MiB, 0)
