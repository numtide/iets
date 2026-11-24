#!/usr/bin/env python3
"""Re-request CodeRabbit reviews on pull requests that hit its rate limit."""

import argparse
import json
import os
import subprocess
from collections.abc import Iterator
from typing import Any

BOT = "coderabbitai"
NOTICE_MARKER = "rate limited by coderabbit.ai"
COMMAND = "@coderabbitai review"
# Anything else after the mention is treated as chat and starts no review.
REVIEW_COMMANDS = {COMMAND, "@coderabbitai full review"}

QUERY = """
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: 25, after: $cursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        comments(last: 100) {
          nodes { author { login } createdAt updatedAt body }
        }
        reviews(last: 50) {
          nodes { author { login } submittedAt }
        }
      }
    }
  }
}
"""


def gh(*args: str) -> str:
    return subprocess.run(
        ["gh", *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def open_pull_requests(repo: str) -> Iterator[dict[str, Any]]:
    owner, name = repo.split("/", 1)
    cursor = None
    while True:
        args = ["-f", f"owner={owner}", "-f", f"name={name}", "-f", f"query={QUERY}"]
        if cursor:
            args += ["-f", f"cursor={cursor}"]
        page = json.loads(gh("api", "graphql", *args))
        pulls = page["data"]["repository"]["pullRequests"]
        yield from pulls["nodes"]
        if not pulls["pageInfo"]["hasNextPage"]:
            return
        cursor = pulls["pageInfo"]["endCursor"]


def is_bot(node: dict[str, Any]) -> bool:
    # author is null for deleted accounts
    return (node["author"] or {}).get("login") == BOT


def pending_since(pr: dict[str, Any]) -> str | None:
    """Return when the PR was rate limited, or None if nothing is pending.

    CodeRabbit keeps the notice in its summary comment after a later review
    succeeds, so only a notice newer than every review and every retry request
    is still waiting. ISO 8601 UTC timestamps compare correctly as strings.
    """
    comments = pr["comments"]["nodes"]
    notices = [
        c["updatedAt"] for c in comments if is_bot(c) and NOTICE_MARKER in c["body"]
    ]
    if not notices:
        return None
    since = max(notices)
    if any(is_bot(r) and r["submittedAt"] > since for r in pr["reviews"]["nodes"]):
        return None
    if any(
        c["body"].strip().lower() in REVIEW_COMMANDS and c["createdAt"] > since
        for c in comments
    ):
        return None
    return since


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY"),
        required="GITHUB_REPOSITORY" not in os.environ,
    )
    parser.add_argument(
        "--max",
        type=int,
        default=2,
        help="PRs to re-request per run; the free OSS quota only hands out a "
        "few reviews per hour, so more just produces fresh notices",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    pending = [
        (since, pr["number"])
        for pr in open_pull_requests(args.repo)
        if (since := pending_since(pr))
    ]
    # Oldest first: a retry that is limited again refreshes the notice and
    # moves to the back of the queue.
    pending.sort()
    print(f"{len(pending)} PRs waiting for a CodeRabbit review")
    for since, number in pending[: max(args.max, 0)]:
        print(f"Re-requesting review on #{number} (rate limited since {since})")
        if not args.dry_run:
            gh("pr", "comment", str(number), "--repo", args.repo, "--body", COMMAND)


if __name__ == "__main__":
    main()
