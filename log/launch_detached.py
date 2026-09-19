"""Detached process launcher: avoids console-handle inheritance so the child
survives the parent shell exit and receives no CTRL_CLOSE shutdown event.
Usage: launch_detached.py <exe> <out.log> <err.log> [args...] -> prints PID:<n>"""
import subprocess
import sys

exe = sys.argv[1]
out_path = sys.argv[2]
err_path = sys.argv[3]
args = sys.argv[4:]
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
