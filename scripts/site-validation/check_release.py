#!/usr/bin/env python3
"""Hold website deployment until its matching stable release is installable."""

import argparse
from dataclasses import dataclass
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tomllib
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

ARCHIVES = ("diskr-aarch64-apple-darwin.tar.gz", "diskr-x86_64-apple-darwin.tar.gz")
SMALL_ASSETS = {"SHA256SUMS": 64 * 1024, "distribution.json": 32 * 1024,
                "install-diskr.sh": 256 * 1024, "diskr.rb": 64 * 1024}
TRUSTED_HOSTS = {"api.github.com", "github.com", "release-assets.githubusercontent.com",
                 "objects.githubusercontent.com"}


class ReleaseCheckError(Exception):
    pass


@dataclass(frozen=True)
class Response:
    status: int
    body: bytes


@dataclass(frozen=True)
class Readiness:
    deploy: bool
    message: str


def trusted_url(url):
    parsed = urlsplit(url)
    if (parsed.scheme != "https" or parsed.hostname not in TRUSTED_HOSTS
            or parsed.username or parsed.password or parsed.port not in (None, 443)):
        raise ReleaseCheckError("Release download redirected outside trusted HTTPS hosts")
    return parsed


class SafeRedirect(HTTPRedirectHandler):
    def redirect_request(self, request, file, code, message, headers, new_url):
        destination = trusted_url(new_url)
        redirected = super().redirect_request(request, file, code, message, headers, new_url)
        if redirected is not None and destination.hostname != "api.github.com":
            redirected.remove_header("Authorization")
        return redirected


def github_fetcher(token):
    opener = build_opener(SafeRedirect())

    def fetch(url, limit):
        parsed = trusted_url(url)
        headers = {"User-Agent": "diskr-website-release-check"}
        if parsed.hostname == "api.github.com":
            headers["Accept"] = "application/vnd.github+json"
            headers["X-GitHub-Api-Version"] = "2022-11-28"
            if token:
                headers["Authorization"] = f"Bearer {token}"
        try:
            with opener.open(Request(url, headers=headers), timeout=20) as response:
                return Response(response.status, response.read(limit + 1))
        except HTTPError as error:
            error.close()
            return Response(error.code, b"")
        except (OSError, URLError) as error:
            raise ReleaseCheckError(f"Release request failed: {type(error).__name__}") from error

    return fetch


def read_url(fetch, url, limit):
    response = fetch(url, limit)
    if response.status == 404:
        return None
    if response.status != 200:
        raise ReleaseCheckError(f"Release request returned HTTP {response.status}: {url}")
    if not isinstance(response.body, bytes) or len(response.body) > limit:
        raise ReleaseCheckError(f"Release response exceeds its size bound: {url}")
    return response.body


def read_json(body, label):
    try:
        value = json.loads(body)
    except (ValueError, UnicodeError) as error:
        raise ReleaseCheckError(f"Malformed {label}") from error
    if not isinstance(value, dict):
        raise ReleaseCheckError(f"Malformed {label}: expected an object")
    return value


def stable_version(version):
    if not isinstance(version, str) or not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
        raise ReleaseCheckError("Cargo package version must be a stable X.Y.Z release")
    return version


def check_release(version, repository, fetch):
    version = stable_version(version)
    if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+", repository)
            or repository.split("/")[1] in (".", "..")):
        raise ReleaseCheckError("Invalid GitHub repository name")
    tag = f"v{version}"
    api = f"https://api.github.com/repos/{repository}/releases/tags/{tag}"
    body = read_url(fetch, api, 1024 * 1024)
    if body is None:
        return Readiness(False, f"HOLD: {repository} has no published {tag} release yet.")
    release = read_json(body, "release metadata")
    if release.get("tag_name") != tag:
        raise ReleaseCheckError("Release tag does not match the Cargo package version")
    if type(release.get("draft")) is not bool or type(release.get("prerelease")) is not bool:
        raise ReleaseCheckError("Release draft/prerelease metadata is invalid")
    if "published_at" not in release:
        raise ReleaseCheckError("Release publication metadata is missing")
    if release["draft"] or release["prerelease"] or release["published_at"] is None:
        return Readiness(False, f"HOLD: {tag} is not a published stable release.")
    try:
        published = datetime.fromisoformat(release["published_at"].replace("Z", "+00:00"))
        if published.tzinfo is None:
            raise ValueError("missing timezone")
    except (AttributeError, TypeError, ValueError) as error:
        raise ReleaseCheckError("Release publication metadata is invalid") from error
    if not isinstance(release.get("assets"), list):
        raise ReleaseCheckError("Release asset metadata is invalid")
    assets = {}
    for asset in release["assets"]:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise ReleaseCheckError("Release asset entry is invalid")
        if asset["name"] in assets:
            raise ReleaseCheckError(f"Duplicate release asset: {asset['name']}")
        assets[asset["name"]] = asset

    def available(names):
        missing = []
        for name in sorted(names):
            asset = assets.get(name)
            if asset is None:
                missing.append(name)
                continue
            if type(asset.get("size")) is not int or asset["size"] < 0 or not isinstance(asset.get("state"), str):
                raise ReleaseCheckError(f"Invalid asset size/state metadata: {name}")
            if asset["state"] != "uploaded" or asset["size"] == 0:
                missing.append(name)
        return missing

    required = set(ARCHIVES) | SMALL_ASSETS.keys()
    missing = available(required)
    if missing:
        return Readiness(False, f"HOLD: {tag} is awaiting uploaded assets: {', '.join(missing)}.")
    downloaded = {}
    for name, limit in SMALL_ASSETS.items():
        url = f"https://github.com/{repository}/releases/download/{tag}/{name}"
        content = read_url(fetch, url, limit)
        if content is None:
            return Readiness(False, f"HOLD: {tag} asset is not downloadable yet: {name}.")
        if len(content) != assets[name]["size"]:
            raise ReleaseCheckError(f"Downloaded asset size disagrees with release metadata: {name}")
        downloaded[name] = content
    metadata = read_json(downloaded["distribution.json"], "distribution metadata")
    if type(metadata.get("apple_notarized")) is not bool:
        raise ReleaseCheckError("Distribution notarization policy is invalid")
    signed = metadata["apple_notarized"]
    expected = {
        "version": version,
        "minimum_macos": "15.0",
        "developer_id_signed": signed,
        "apple_notarized": signed,
        "stapled_disk_images": signed,
        "standalone_binary_ticket": "online" if signed else "unavailable",
        "archives": sorted(ARCHIVES),
    }
    for key, value in expected.items():
        if type(metadata.get(key)) is not type(value) or metadata[key] != value:
            raise ReleaseCheckError(f"Distribution metadata disagrees with release policy: {key}")
    if signed:
        required.update(name.removesuffix(".tar.gz") + ".dmg" for name in ARCHIVES)
        missing = available(required)
        if missing:
            return Readiness(False, f"HOLD: notarized {tag} is awaiting disk images: {', '.join(missing)}.")
    checksums = {}
    try:
        for line in downloaded["SHA256SUMS"].decode("ascii").splitlines():
            match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *]([A-Za-z0-9_.-]+)", line)
            if not match or match[2] in checksums:
                raise ValueError("invalid or duplicate checksum")
            checksums[match[2]] = match[1].lower()
    except (UnicodeError, ValueError) as error:
        raise ReleaseCheckError("Invalid release checksums") from error
    if set(checksums) != required - {"SHA256SUMS"}:
        raise ReleaseCheckError("Release checksums do not cover the required distribution assets")
    for name in SMALL_ASSETS.keys() - {"SHA256SUMS"}:
        if hashlib.sha256(downloaded[name]).hexdigest() != checksums[name]:
            raise ReleaseCheckError(f"Release checksum mismatch: {name}")
    installer = downloaded["install-diskr.sh"].decode("utf-8")
    if len(re.findall(rf"^release_version='{re.escape(version)}'$", installer, re.M)) != 1:
        raise ReleaseCheckError("Released installer is not pinned to the matching version")
    return Readiness(True, f"READY: {tag} has uploaded native archives and verified installation metadata, installer and formula.")


def emit(result, environment):
    print(result.message)
    if environment.get("GITHUB_OUTPUT"):
        with Path(environment["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            output.write(f"deploy={'true' if result.deploy else 'false'}\n")
    if environment.get("GITHUB_STEP_SUMMARY"):
        with Path(environment["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as summary:
            summary.write(f"### Website release availability\n\n{result.message}\n\n")


def main(argv=None, fetch=None, environment=None):
    environment = os.environ if environment is None else environment
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path(__file__).resolve().parents[2] / "Cargo.toml")
    parser.add_argument("--repository", default=environment.get("GITHUB_REPOSITORY", "milobeans/diskr"))
    args = parser.parse_args(argv)
    try:
        with args.manifest.open("rb") as manifest:
            version = tomllib.load(manifest)["package"]["version"]
        result = check_release(version, args.repository, fetch or github_fetcher(environment.get("GH_TOKEN")))
    except (ReleaseCheckError, OSError, UnicodeError, ValueError, KeyError, TypeError) as error:
        emit(Readiness(False, f"ERROR: release readiness could not be verified: {error}"), environment)
        return 1
    emit(result, environment)
    return 0


if __name__ == "__main__":
    sys.exit(main())
