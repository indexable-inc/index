"""Exercise worker assignment events using the package's native evaluator."""
import json
import os
import subprocess
import sys
import tempfile

EXPRESSION = '{ smoke = derivation { name = "telemetry-smoke"; system = "x86_64-linux"; builder = "/bin/false"; }; }'


def check_evaluator(environment: dict[str, str]) -> None:
    argv = [sys.argv[1], "--workers", "1", "--no-instantiate", "--expr", EXPRESSION]
    ordinary = subprocess.run(argv, check=True, stdout=subprocess.PIPE, text=True, env=environment | {"JJ_EVAL_PROGRESS": "0"})
    rows = [json.loads(line) for line in ordinary.stdout.splitlines()]
    assert len(rows) == 1
    assert rows[0]["attrPath"] == ["smoke"]
    observed = subprocess.run(argv, check=True, stdout=subprocess.PIPE, text=True, env=environment | {"JJ_EVAL_PROGRESS": "1"})
    rows = [json.loads(line) for line in observed.stdout.splitlines()]
    contexts = [row["evaluation_cache"] for row in rows if row.get("type") == "evaluation-context"]
    assert len(contexts) == 1
    assert contexts[0]["engine"] == "nix-eval-rs"
    assert contexts[0]["mode"] == "worker-local"
    assert contexts[0]["stats"] is None
    assert contexts[0]["reason"]
    assert contexts[0]["executable"] == os.path.realpath(sys.argv[1])
    start = next(index for index, row in enumerate(rows) if row.get("event") == "start" and row["attrPath"] == ["smoke"])
    result = next(index for index, row in enumerate(rows) if "drvPath" in row)
    finish = next(index for index, row in enumerate(rows) if row.get("event") == "finish" and row["attrPath"] == ["smoke"])
    assert start < result < finish
    assert rows[start]["worker"] == rows[finish]["worker"] > 0
    assert rows[start]["at_ms"] <= rows[finish]["at_ms"]
    print("2/2 native evaluator controls pass: opt-in activity and unchanged default JSONL")


def main() -> None:
    # Package checks run with a read-only store and HOME=/homeless-shelter.
    # This expression needs no store writes or substitutes.
    with tempfile.TemporaryDirectory() as directory:
        check_evaluator(os.environ | {"HOME": directory, "XDG_CACHE_HOME": directory, "NIX_REMOTE": "dummy://"})


if __name__ == "__main__":
    main()
