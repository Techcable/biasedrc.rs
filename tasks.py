import os
import shlex
import sys

from invoke import Collection, task

HAS_COLORS: bool = (sys.stderr.isatty() or os.getenv("CLICOLOR_FORCE")) and not os.getenv("NO_COLOR")


def apply_colors(msg: object, /, *, code: str) -> str:
    if HAS_COLORS:
        return f"\x1b[{code}m{msg}\x1b[0m"
    else:
        return str(msg)


def log_info(msg: object):
    print(
        apply_colors("INFO:", code="1;32"),
        apply_colors(msg, code="1"),
    )


@task
def test(ctx):
    check(ctx, format=False)
    ctx.run("cargo nextest run --workspace", pty=True)
    miri(ctx)
    run_format(ctx, check=True)


@task
def coverage(ctx):
    check(ctx, format=False)
    ctx.run("cargo +nightly llvm-cov nextest --all-features", pty=True)
    run_format(ctx, check=True)


@task
def miri(ctx):
    # https://docs.pyinvoke.org/en/3.0/concepts/invoking-tasks.html#wrapper-tasks
    ctx.run(f"cargo +nightly miri nextest run --all-features {ctx.remainder}", pty=True)


@task
def bench(ctx, basic=False):
    args = shlex.split(ctx.remainder)
    if basic:
        args.append("--bench=basic")
    ctx.run(shlex.join([*shlex.split("cargo +nightly bench -p benchmark-biasedrc --all-features"), *args]), pty=True)


@task
def check(ctx, format=True):
    clippy(ctx)
    doc(ctx)
    # by default, check formatting as well
    if format:
        run_format(ctx, check=True)


@task
def clippy(ctx):
    ctx.run("cargo +nightly clippy --workspace --all-targets", pty=True)
    ctx.run("cargo +nightly clippy --workspace --all-targets", pty=True)


@task
def doc(ctx):
    """Build documentation and check for issues"""
    ctx.run("cargo doc --workspace --document-private-items --no-deps", pty=True)


@task(name="format")
def run_format(ctx, check=False):
    verb = "Checking" if check else "Fixing"
    log_info(f"{verb} formatting")
    maybe_check = " --check" if check else ""
    maybe_fix = " --fix" if not check else ""
    ctx.run("cargo +nightly fmt --all" + maybe_check)
    ctx.run("taplo format" + maybe_check)
    ctx.run("cargo sort --grouped --no-format --workspace" + maybe_check)

    # need python format for invoke.py
    ctx.run("ruff format" + maybe_check)
    ctx.run("ruff check --select=I" + maybe_fix)  # works like isort
    check_spelling(ctx, fix=False)


TYPOS_VER = "1.46"  # pinned to avoid update breakage


@task(name="typos")
def check_spelling(ctx, fix=False):
    maybe_write = " --write-changes" if fix else ""
    ctx.run(f"uvx typos@{TYPOS_VER}" + maybe_write)


ns = Collection(test, coverage, miri, bench, check, clippy, doc, run_format, check_spelling)
ns.configure(
    {
        "run": {
            "echo": True,
            "env": {
                "CLICOLOR_FORCE": "1" if HAS_COLORS else "0",
                "RUST_BACKTRACE": "1",
                # Due to header arithmetic, we had problems with stacked borrows in the past.
                # Right now it seems to be working. If problems come up switch back to -Zmiri-tree-borrows.
                "MIRIFLAGS": "-Zmiri-strict-provenance -Zmiri-env-forward=RUST_BACKTRACE",
            },
        }
    }
)
