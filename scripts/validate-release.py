#!/usr/bin/env python3
"""Validate a release tag against the checked-out commit and source branch."""
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path


def validate(tag: str, sha: str) -> tuple[str, bool]:
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?", tag)
    if not match or len(tag) > 121:
        raise ValueError("tag must be a valid vMAJOR.MINOR.PATCH Docker tag with an optional prerelease")
    suffix = match.group(4)
    if suffix and any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in suffix.split(".")):
        raise ValueError("numeric prerelease identifiers cannot have leading zeroes")
    with Path("Cargo.toml").open("rb") as cargo:
        version = tomllib.load(cargo)["package"]["version"]
    if tag[1:] != version:
        raise ValueError(f"tag {tag} does not match Cargo package version {version}")
    resolved = subprocess.check_output(["git", "rev-parse", f"refs/tags/{tag}^{{commit}}"], text=True).strip()
    if resolved != sha:
        raise ValueError("tag does not resolve to the checked-out commit")
    branch = "develop" if match.group(1) == "0" else "main"
    subprocess.run(["git", "fetch", "--no-tags", "origin", f"+refs/heads/{branch}:refs/remotes/origin/{branch}"], check=True)
    if subprocess.run(["git", "merge-base", "--is-ancestor", sha, f"refs/remotes/origin/{branch}"], check=False).returncode:
        raise ValueError(f"tagged commit is not reachable from origin/{branch}")
    return branch, match.group(1) == "0" or suffix is not None


if __name__ == "__main__":
    try:
        if os.environ["GITHUB_REF_TYPE"] != "tag":
            raise ValueError("select a tag for this workflow")
        branch, prerelease = validate(os.environ["GITHUB_REF_NAME"], os.environ["GITHUB_SHA"])
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            print(f"prerelease={str(prerelease).lower()}", file=output)
        print(f"Validated tag and commit against origin/{branch}")
    except (ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(f"Release validation failed: {error}")
