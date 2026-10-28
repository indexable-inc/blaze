"""Validate the selected downstream build before any of its code or artifacts run.

Run with the root uv workspace and one typed JSON input: repository, build_run, sha,
and output (GITHUB_OUTPUT). The dispatch commit must match the build commit; dispatch
downstream.yaml on the same branch or a retained tag when testing an older build.
"""

import asyncio
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import cast

from typed_input import fail, parse

SCRIPT = "downstream_build.py"
WORKFLOWS = {".github/workflows/focused.yaml", ".github/workflows/train.yaml"}


@dataclass(frozen=True)
class Build:
    repository: str
    build_run: str
    sha: str
    output: str


class Failed(Exception):
    pass


def require(ok: bool, why: str) -> None:
    if not ok:
        raise Failed(why)


def field(value: object, key: str) -> object:
    require(isinstance(value, dict), f"GitHub response for {key}: expected an object")
    return cast(dict[str, object], value).get(key)


def items(value: object, key: str) -> list[object]:
    result = field(value, key)
    require(isinstance(result, list), f"GitHub response for {key}: expected a list")
    return cast(list[object], result)


def append_output(path: str, text: str) -> None:
    with Path(path).open("a") as output:
        output.write(text)


def failure_messages(group: BaseExceptionGroup[Failed]) -> str:
    return "\n".join(
        failure_messages(error) if isinstance(error, BaseExceptionGroup) else str(error)
        for error in group.exceptions
    )


async def api(repository: str, endpoint: str) -> object:
    route = f"repos/{repository}" + (f"/{endpoint}" if endpoint else "")
    argv = ["gh", "api", route]
    step = f"build provenance: {' '.join(argv)}"
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
    except OSError as e:
        raise Failed(f"{step}: could not start (no exit code): {e}") from e
    try:
        async with asyncio.timeout(60):
            stdout, stderr = await proc.communicate()
    except (TimeoutError, asyncio.CancelledError) as e:
        proc.kill()
        _, stderr = await proc.communicate()
        if isinstance(e, asyncio.CancelledError):
            raise
        raise Failed(f"{step}: timed out, exit {proc.returncode}: {stderr.decode(errors='replace')[-3000:]}")
    require(
        proc.returncode == 0,
        f"{step}: exit {proc.returncode}: {stderr.decode(errors='replace')[-3000:]}",
    )
    try:
        value: object = json.loads(stdout)
    except json.JSONDecodeError as e:
        raise Failed(f"{step}: response is not JSON: {e}") from e
    return value


async def successful_build(it: Build) -> None:
    # Jobs are paginated; the full train has more than one page. The endpoint's default
    # filter is the latest attempt, so a successful job from an older rerun cannot qualify.
    page = 1
    found = 0
    while True:
        response = await api(it.repository, f"actions/runs/{it.build_run}/jobs?per_page=100&page={page}")
        jobs = items(response, "jobs")
        for job in jobs:
            if field(job, "name") == "linux x64 / build":
                require(field(job, "head_sha") == it.sha, "Linux x64 build job has another commit")
                require(
                    field(job, "status") == "completed" and field(job, "conclusion") == "success",
                    "Linux x64 build job has not completed successfully",
                )
                found += 1
        if len(jobs) < 100:
            break
        page += 1
    require(found == 1, f"expected one successful Linux x64 build job, found {found}")


async def validate(it: Build) -> None:
    require(
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", it.repository) is not None,
        "repository: owner/repo required",
    )
    require(
        re.fullmatch(r"[1-9][0-9]*", it.build_run) is not None,
        "build_run: positive decimal run ID required",
    )
    require(re.fullmatch(r"[0-9a-f]{40}", it.sha) is not None, "sha: full lowercase commit SHA required")
    async with asyncio.TaskGroup() as group:
        repo_task = group.create_task(api(it.repository, ""))
        run_task = group.create_task(api(it.repository, f"actions/runs/{it.build_run}"))
    repo, run = repo_task.result(), run_task.result()
    repository_id = field(repo, "id")
    require(type(repository_id) is int and repository_id > 0, "repository: missing numeric GitHub ID")
    require(field(run, "id") == int(it.build_run), "selected run has another ID")
    require(
        field(field(run, "repository"), "id") == repository_id,
        "selected run belongs to another repository",
    )
    require(field(field(run, "head_repository"), "id") == repository_id, "selected run's source is a fork")
    require(field(run, "event") == "workflow_dispatch", "selected run must be a manually dispatched build")
    workflow_path = field(run, "path")
    require(
        isinstance(workflow_path, str) and workflow_path in WORKFLOWS,
        "selected run must be focused.yaml or train.yaml",
    )
    require(
        field(run, "head_sha") == it.sha,
        "selected build commit differs from this dispatch; use --ref with the build's branch "
        "or a retained tag at that commit (the branch may have moved)",
    )
    workflow_id = field(run, "workflow_id")
    require(type(workflow_id) is int and workflow_id > 0, "selected run has no numeric workflow ID")
    async with asyncio.TaskGroup() as group:
        workflow_task = group.create_task(api(it.repository, f"actions/workflows/{workflow_id}"))
        artifact_task = group.create_task(
            api(it.repository, f"actions/runs/{it.build_run}/artifacts?name=blaze-linux-x64&per_page=100")
        )
        group.create_task(successful_build(it))
    require(
        field(workflow_task.result(), "path") == workflow_path,
        "selected run's workflow identity differs",
    )
    artifacts = artifact_task.result()
    matches = items(artifacts, "artifacts")
    require(
        field(artifacts, "total_count") == 1 and len(matches) == 1,
        "expected exactly one blaze-linux-x64 artifact",
    )
    artifact = matches[0]
    require(field(artifact, "name") == "blaze-linux-x64", "selected artifact has another name")
    require(field(artifact, "expired") is False, "selected build artifact has expired")
    digest = field(artifact, "digest")
    require(
        isinstance(digest, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is not None,
        "selected build artifact has no SHA-256 digest",
    )
    provenance = field(artifact, "workflow_run")
    for key, expected in (
        ("id", int(it.build_run)),
        ("repository_id", repository_id),
        ("head_repository_id", repository_id),
        ("head_sha", it.sha),
    ):
        require(
            field(provenance, key) == expected,
            f"selected artifact's {key} differs from the validated build",
        )
    artifact_id = field(artifact, "id")
    require(type(artifact_id) is int and artifact_id > 0, "selected artifact has no numeric ID")
    await asyncio.to_thread(append_output, it.output, f"run-id={it.build_run}\nartifact-id={artifact_id}\n")
    print(f"Validated {workflow_path} run {it.build_run}, commit {it.sha}, artifact {artifact_id}")


def main() -> None:
    if len(sys.argv) != 2:
        fail(SCRIPT, f"usage: {SCRIPT} JSON")
    try:
        asyncio.run(validate(parse(Build, sys.argv[1], SCRIPT)))
    except* Failed as group:
        fail(SCRIPT, failure_messages(group))


if __name__ == "__main__":
    main()
