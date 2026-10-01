#!/usr/bin/env python3
"""Headless test of the actual apphost and dynamically loaded example config."""
import argparse
import json
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--configuration", default="Debug")
    args = parser.parse_args()
    runtime = ROOT / f"dotnet/ShojiWM.Runtime/bin/{args.configuration}/net10.0/ShojiWM.Runtime"
    config = ROOT / f"dotnet/ShojiWM.Example/bin/{args.configuration}/net10.0/ShojiWM.Example.dll"
    snapshot = json.loads((ROOT / "dotnet/fixtures/window.json").read_text())

    # Build all requests upfront, then communicate with a hard deadline. This
    # tests framing/assembly loading without depending on a Wayland session.
    def request(index, kind, **fields):
        return dict(requestId=index, kind=kind, nowMs=1234, displayState={}, inputState={}, **fields)

    requests = [request(1, "drainPreload"), request(2, "lifecycleEnable", reason="initial"),
                request(3, "evaluate", snapshot=snapshot, windowId="1")]
    for _ in range(12):
        for kind, fields in [
            ("prepareAssembly", dict(configPath=str(config))),
            ("evaluateCandidatePreview", dict(snapshot=snapshot, windowId="1")),
            ("commitAssembly", {}),
            ("evaluate", dict(snapshot=snapshot, windowId="1")),
        ]:
            requests.append(request(len(requests) + 1, kind, **fields))
    requests.append(request(len(requests) + 1, "shutdownAssemblies", reason="shutdown"))
    result = subprocess.run([str(runtime), "--config", str(config)],
                            input="".join(json.dumps(q) + "\n" for q in requests),
                            capture_output=True, text=True, timeout=30, check=True)
    responses = [json.loads(line) for line in result.stdout.splitlines()]
    assert len(responses) == len(requests), result.stdout
    for request_value, response in zip(requests, responses):
        assert response["ok"] and response["requestId"] == request_value["requestId"]
        assert response["kind"] == request_value["kind"]

    def nodes(node):
        yield node
        for child in node.get("children", []):
            yield from nodes(child)

    tree = responses[2]["serialized"]
    assert tree["kind"] == "WindowBorder"
    assert sum(node["kind"] == "Window" for node in nodes(tree)) == 1
    label = next(node for node in nodes(tree) if node["kind"] == "Label")["props"]["text"]
    assert snapshot["title"] in label and snapshot["appId"] in label
    handler = next(node for node in nodes(tree) if node["kind"] == "Button")["props"]["onClick"]
    assert handler["kind"] == "runtime-handler" and handler["id"].startswith("handler-")
    assert "config enabled" in result.stderr
    rendered = [response["serialized"] for response in responses if response["kind"] == "evaluate"]
    ids = [next(node for node in nodes(tree) if node["kind"] == "Button")["props"]["onClick"]["id"] for tree in rendered]
    assert len(ids) == 13 and len(set(ids)) == 13
    assert "ALC(s) still referenced" not in result.stderr, result.stderr
    print("PASS actual .NET worker: lifecycle, NDJSON, tree, 12 assembly reloads in one process, isolated handler IDs, ALC cleanup")


if __name__ == "__main__":
    main()
