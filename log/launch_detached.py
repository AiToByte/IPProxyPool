"""Detached process launcher: avoids console-handle inheritance so the child
survives the parent shell exit and receives no CTRL_CLOSE shutdown event.
Usage: launch_detached.py <exe> <out.log> <err.log> [args...] -> prints PID:<n>

C2 log rotation: at launch, any target >50MB is moved aside to `<path>.1`
(previous `.1` removed first, so at most 2 generations are kept). Rotation
happens once per launch, not mid-run (long-lived children keep appending).
"""
import os
import subprocess
import sys

ROTATE_BYTES = 50 * 1024 * 1024


def maybe_rotate(path):
    try:
        if os.path.getsize(path) <= ROTATE_BYTES:
            return
    except OSError:
        return
    backup = path + ".1"
    try:
        if os.path.exists(backup):
            os.remove(backup)
        os.rename(path, backup)
    except OSError as e:
        sys.stderr.write(f"[launch_detached] rotate {path}: {e}\n")


exe = sys.argv[1]
out_path = sys.argv[2]
err_path = sys.argv[3]
args = sys.argv[4:]
maybe_rotate(out_path)
maybe_rotate(err_path)
out = open(out_path, "ab", buffering=0)
err = open(err_path, "ab", buffering=0)
p = subprocess.Popen(
    [exe, *args],
    stdout=out,
    stderr=err,
    stdin=subprocess.DEVNULL,
    creationflags=subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP,
    close_fds=True,
)
print(f"PID:{p.pid}")
