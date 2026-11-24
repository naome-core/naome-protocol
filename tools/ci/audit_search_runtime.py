"""Check the installed search runtime and its active dependencies against OSV."""

from importlib.metadata import distribution
import json
from pathlib import Path
import time
from urllib.error import URLError
from urllib.request import Request, urlopen

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name
from packaging.version import Version


def requirements(path):
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.startswith("-r "):
            yield from requirements(path.parent / line[3:])
        elif line and not line.startswith("#"):
            yield Requirement(line)


def installed_packages(roots):
    pending = [(req, "") for req in roots]
    visited = set()
    versions = {}
    while pending:
        req, parent_extra = pending.pop()
        if req.marker and not req.marker.evaluate({"extra": parent_extra}):
            continue
        name = canonicalize_name(req.name)
        dist = distribution(name)
        version = Version(dist.version)
        if req.specifier and version not in req.specifier:
            raise ValueError(f"{name} {version} does not satisfy {req}")
        # OSV indexes the upstream public version of local builds such as CPU wheels.
        versions[name] = version.public
        for extra in {""} | req.extras:
            if (name, extra) not in visited:
                visited.add((name, extra))
                pending.extend((Requirement(dep), extra) for dep in dist.requires or [])
    return sorted(versions.items())


def main():
    inputs = Path(__file__).resolve().parents[2] / "requirements-search.txt"
    packages = installed_packages(requirements(inputs))
    query = {"queries": [
        {"package": {"ecosystem": "PyPI", "name": name}, "version": version}
        for name, version in packages
    ]}
    request = Request(
        "https://api.osv.dev/v1/querybatch",
        data=json.dumps(query).encode(),
        headers={"Content-Type": "application/json"},
    )
    for attempt in range(3):
        try:
            with urlopen(request, timeout=30) as response:
                results = json.load(response)["results"]
            break
        except (TimeoutError, URLError):
            if attempt == 2:
                raise
            time.sleep(2 ** attempt)
    findings = []
    for (name, version), result in zip(packages, results, strict=True):
        if not isinstance(result, dict) or result.keys() - {"vulns", "next_page_token"}:
            raise ValueError(f"unexpected OSV result for {name} {version}")
        ids = [item["id"] for item in result.get("vulns", [])]
        if result.get("next_page_token") and not ids:
            raise ValueError(f"incomplete OSV result for {name} {version}")
        if ids:
            findings.append(f"{name} {version}: {', '.join(ids)}")
    if findings:
        raise SystemExit("\n".join(findings))
    print(f"OSV checked {len(packages)} installed search runtime packages.")


if __name__ == "__main__":
    main()
