#!/usr/bin/env python3
"""Generate a large sample repository for benchmarking pando.

Creates, under the output directory:

  origin.git   a bare "remote" with a long main history and many topic branches
  repo/        a clone of origin with many local branches and linked worktrees

History is written with `git fast-import`, so even large shapes build in
seconds. Everything is deterministic for a given set of arguments.
"""

import argparse
import os
import random
import shutil
import subprocess
import sys


def run(*args, cwd=None, stdin=None):
    subprocess.run(args, cwd=cwd, input=stdin, check=True,
                   stdout=subprocess.DEVNULL)


def blob(rng, size):
    words = ["alpha", "beta", "gamma", "delta", "tree", "root", "leaf", "trunk"]
    out = []
    total = 0
    while total < size:
        line = " ".join(rng.choice(words) for _ in range(8)) + "\n"
        out.append(line)
        total += len(line)
    return "".join(out).encode()


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("out", help="output directory (replaced if it exists)")
    p.add_argument("--files", type=int, default=5000, help="tracked files")
    p.add_argument("--commits", type=int, default=2000, help="main commits")
    p.add_argument("--remote-branches", type=int, default=1500,
                   help="topic branches published to origin")
    p.add_argument("--local-branches", type=int, default=500,
                   help="local branches created in the clone")
    p.add_argument("--worktrees", type=int, default=40, help="linked worktrees")
    p.add_argument("--dirty", type=int, default=10,
                   help="worktrees left with uncommitted changes")
    p.add_argument("--ignored-mb", type=int, default=20,
                   help="ignored build output per worktree, in MiB")
    p.add_argument("--seed", type=int, default=1)
    a = p.parse_args()

    rng = random.Random(a.seed)
    out = os.path.abspath(a.out)
    if os.path.exists(out):
        shutil.rmtree(out)
    os.makedirs(out)
    origin = os.path.join(out, "origin.git")
    repo = os.path.join(out, "repo")
    run("git", "init", "-q", "--bare", "-b", "main", origin)

    # fast-import stream: main history, then topic branches off random commits.
    stream = []
    ident = "Bench <bench@example.com>"
    t = 1_600_000_000
    paths = [f"src/mod{i % 97:02d}/file{i:05d}.txt" for i in range(a.files)]
    stream.append(b"commit refs/heads/main\nmark :1\n")
    stream.append(f"committer {ident} {t} +0000\n".encode())
    stream.append(b"data 7\ninitial\n")
    stream.append(b"M 644 inline .gitignore\ndata 7\ntarget\n")
    for path in paths:
        data = blob(rng, rng.randint(200, 4000))
        stream.append(f"M 644 inline {path}\ndata {len(data)}\n".encode())
        stream.append(data + b"\n")
    for c in range(2, a.commits + 1):
        t += 60
        msg = f"change {c}".encode()
        stream.append(f"commit refs/heads/main\nmark :{c}\n".encode())
        stream.append(f"committer {ident} {t} +0000\n".encode())
        stream.append(f"data {len(msg)}\n".encode() + msg + b"\n")
        stream.append(f"from :{c - 1}\n".encode())
        for path in rng.sample(paths, 3):
            data = blob(rng, rng.randint(200, 4000))
            stream.append(f"M 644 inline {path}\ndata {len(data)}\n".encode())
            stream.append(data + b"\n")
    mark = a.commits
    for b in range(a.remote_branches):
        parent = rng.randint(max(1, a.commits - 500), a.commits)
        for k in range(rng.randint(1, 3)):
            mark += 1
            t += 60
            msg = f"topic {b} step {k}".encode()
            stream.append(f"commit refs/heads/topic/{b:05d}\nmark :{mark}\n".encode())
            stream.append(f"committer {ident} {t} +0000\n".encode())
            stream.append(f"data {len(msg)}\n".encode() + msg + b"\n")
            stream.append(f"from :{parent}\n".encode())
            path = rng.choice(paths)
            data = blob(rng, 500)
            stream.append(f"M 644 inline {path}\ndata {len(data)}\n".encode())
            stream.append(data + b"\n")
            parent = mark
    run("git", "fast-import", "--quiet", cwd=origin, stdin=b"".join(stream))
    run("git", "gc", "-q", cwd=origin)

    run("git", "clone", "-q", origin, repo)
    run("git", "config", "user.name", "Bench", cwd=repo)
    run("git", "config", "user.email", "bench@example.com", cwd=repo)
    # Local branches: some track origin topics, some are purely local.
    refs = []
    for b in range(a.local_branches):
        if b % 2 == 0 and b < a.remote_branches:
            refs.append(f"create refs/heads/topic/{b:05d} refs/remotes/origin/topic/{b:05d}\n")
        else:
            refs.append(f"create refs/heads/local/{b:05d} HEAD~{rng.randint(0, 200)}\n")
    run("git", "update-ref", "--stdin", cwd=repo, stdin="".join(refs).encode())
    for b in range(0, min(a.local_branches, a.remote_branches), 2):
        name = f"topic/{b:05d}"
        run("git", "config", f"branch.{name}.remote", "origin", cwd=repo)
        run("git", "config", f"branch.{name}.merge", f"refs/heads/{name}", cwd=repo)

    worktrees = os.path.join(out, "worktrees")
    os.makedirs(worktrees)
    filler = os.urandom(1024 * 1024)
    for w in range(a.worktrees):
        branch = f"local/{2 * w + 1:05d}" if 2 * w + 1 < a.local_branches else f"wt/{w:04d}"
        dest = os.path.join(worktrees, branch.replace("/", "-"))
        args = ["git", "worktree", "add", "-q"]
        if branch.startswith("wt/"):
            args += ["-b", branch]
        run(*args, dest, *([] if branch.startswith("wt/") else [branch]), cwd=repo)
        target = os.path.join(dest, "target")
        os.makedirs(target)
        for i in range(a.ignored_mb):
            with open(os.path.join(target, f"obj{i:03d}.bin"), "wb") as f:
                f.write(filler)
        if w < a.dirty:
            with open(os.path.join(dest, paths[w]), "a") as f:
                f.write("dirty\n")
    print(repo)


if __name__ == "__main__":
    sys.exit(main())
