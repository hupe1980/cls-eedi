#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["jsonschema>=4.22,<5"]
# ///
"""Compare the local validator and the pinned upstream draft-07 example corpus."""

import argparse
import json
import subprocess
import sys
from pathlib import Path

from jsonschema import Draft7Validator
from referencing import Registry
from referencing.jsonschema import DRAFT7

REVISION = "cfa11c0aed20cc28d7ca24ef106082fe4c45b786"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "checkout", type=Path, help="clean KEO specification checkout at 1.2.0"
    )
    options = parser.parse_args()
    base = options.checkout.resolve()
    revision = subprocess.check_output(
        ["git", "-C", base, "rev-parse", "HEAD"], text=True
    ).strip()
    if revision != REVISION or subprocess.check_output(
        ["git", "-C", base, "status", "--porcelain", "--", "schemas"]
    ):
        parser.error("expected the unchanged 1.2.0 schema tree")

    schema_dir = base / "schemas"
    # Upstream uses file: references rooted at schemas/ even inside nested schemas.
    # Resolve just those references to the corresponding declared resource IDs.
    schemas = {
        path.relative_to(schema_dir).as_posix(): json.loads(path.read_bytes())
        for path in sorted(schema_dir.rglob("*.schema.json"))
    }

    def resolve_refs(node):
        if isinstance(node, list):
            for item in node:
                resolve_refs(item)
        if isinstance(node, dict):
            ref = node.get("$ref", "")
            if ref.startswith("file:"):
                filename, fragment, pointer = ref[5:].partition("#")
                node["$ref"] = schemas[filename]["$id"] + fragment + pointer
            for value in node.values():
                resolve_refs(value)

    for schema in schemas.values():
        resolve_refs(schema)
    resources = [
        (schema["$id"], DRAFT7.create_resource(schema)) for schema in schemas.values()
    ]
    validator = Draft7Validator(
        schemas["message.schema.json"], registry=Registry().with_resources(resources)
    )
    root = Path(__file__).resolve().parents[1]
    build = subprocess.check_output(
        ["cargo", "build", "--locked", "--bin", "cls-eedi", "--message-format=json"],
        cwd=root,
        text=True,
    )
    executable = next(
        event["executable"]
        for line in build.splitlines()
        if (event := json.loads(line)).get("executable")
    )
    failures = 0
    paths = sorted((schema_dir / "clseedi" / "examples").glob("*.json"))
    trust_examples = {
        "de.keo-connectivity.clseedi.state.json": (1, 0, 0),
        "de.keo-connectivity.clseedi.state.trustShipPairingServiceDone.json": (0, 0, 1),
        "de.keo-connectivity.clseedi.state.measurements.json": None,
        "de.keo-connectivity.clseedi.state.notify.json": None,
    }
    for path in paths:
        data = path.read_bytes()
        envelope = json.loads(data)
        valid_schema = validator.is_valid(envelope)
        valid_codec = (
            subprocess.run(
                [executable, "check"], input=data, capture_output=True, check=False
            ).returncode
            == 0
        )
        print(f"{path.name}: schema={valid_schema} codec={valid_codec}")
        failures += not (valid_schema and valid_codec)
        if path.name in trust_examples:
            counts = trust_examples.pop(path.name)
            trust = (
                None
                if counts is None
                else dict(
                    zip(
                        ("directSki", "pairingPending", "pairingEstablished"),
                        counts,
                        strict=True,
                    )
                )
            )
            result = subprocess.run(
                [executable, "trust-summary"],
                input=data,
                capture_output=True,
                check=False,
            )
            expected = {"timestamp": envelope["data"]["timestamp"], "trust": trust}
            passed = (
                result.returncode == 0
                and not result.stderr
                and json.loads(result.stdout) == expected
            )
            print(f"{path.name}: trust-summary={passed}")
            failures += not passed
    failures += len(trust_examples)
    print(f"{len(paths)} upstream examples, {failures} failures")
    return bool(failures or not paths)


if __name__ == "__main__":
    sys.exit(main())
