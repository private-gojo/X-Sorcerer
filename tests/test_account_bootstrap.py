#!/usr/bin/env python3
import os
import pathlib
import subprocess
import tempfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
ENTRYPOINT = ROOT / "entrypoint.sh"
SOURCE = ENTRYPOINT.read_text(encoding="utf-8")
START = "# Require explicit credentials; never create a predictable fallback account."
END = "# I generate dropbear host keys if they don't exist yet."

start_at = SOURCE.find(START)
end_at = SOURCE.find(END, start_at + len(START)) if start_at >= 0 else -1
if start_at < 0 or end_at < 0:
    raise SystemExit("Could not locate the account bootstrap block")

BLOCK = SOURCE[start_at:end_at]


def run_block(accounts, path=None, extra_env=None):
    env = os.environ.copy()
    env.pop("ACCOUNTS", None)
    if accounts is not None:
        env["ACCOUNTS"] = accounts
    if path is not None:
        env["PATH"] = f"{path}:{env['PATH']}"
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        ["bash", "-c", BLOCK],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


def expect_rejected(accounts, label):
    result = run_block(accounts)
    if result.returncode == 0:
        raise AssertionError(f"{label}: invalid credentials were accepted")
    if "refusing to start" not in result.stderr and "invalid username/password" not in result.stderr:
        raise AssertionError(f"{label}: unexpected rejection output: {result.stderr!r}")


expect_rejected(None, "unset ACCOUNTS")
expect_rejected("", "empty ACCOUNTS")
for invalid in ("missing-colon", "user:", ":password", "bad user:password",
                "user:password:extra", "user:line1\nline2"):
    expect_rejected(invalid, invalid)

with tempfile.TemporaryDirectory() as temp_dir:
    temp = pathlib.Path(temp_dir)
    calls = temp / "adduser.log"
    passwords = temp / "chpasswd.log"

    adduser = temp / "adduser"
    adduser.write_text(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CALL_LOG\"\n",
        encoding="utf-8",
    )
    adduser.chmod(0o755)

    chpasswd = temp / "chpasswd"
    chpasswd.write_text(
        "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> \"$PASS_LOG\"; done\n",
        encoding="utf-8",
    )
    chpasswd.chmod(0o755)

    result = run_block(
        "alpha:secret-one,beta:secret-two",
        path=temp_dir,
        extra_env={"CALL_LOG": str(calls), "PASS_LOG": str(passwords)},
    )
    if result.returncode != 0:
        raise AssertionError(f"valid accounts rejected: {result.stderr!r}")

    created = calls.read_text(encoding="utf-8").splitlines()
    configured = passwords.read_text(encoding="utf-8").splitlines()
    if created != ["-D -s /bin/false alpha", "-D -s /bin/false beta"]:
        raise AssertionError(f"unexpected adduser calls: {created!r}")
    if configured != ["alpha:secret-one", "beta:secret-two"]:
        raise AssertionError("password bootstrap did not preserve both credentials")

print("PASS: account bootstrap rejects missing/malformed credentials and accepts valid entries")
