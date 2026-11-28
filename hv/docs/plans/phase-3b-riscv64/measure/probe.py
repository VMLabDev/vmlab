#!/usr/bin/env python3
"""card-1242: boot one riscv64 template under QEMU TCG with a chosen -cpu and
the rv64trace plugin, stop at agent ready, and leave the plugin's dump.
usage: probe.py <bench-dir> <guest> <cpu> <smp> <outdir> [plugin.so]"""
import os, shutil, sys, time, json
bench, guest, cpu, smp, outdir = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
plugin = sys.argv[6] if len(sys.argv) > 6 else None
sys.path.insert(0, bench)
import lib, backends

def log(m):
    print(time.strftime("[%H:%M:%S]"), m, flush=True)

tag = f"{guest}-{os.environ.get('TAG', cpu.split(',')[0])}-smp{smp}" + ("-plug" if plugin else "")
run = os.path.join(outdir, tag)
shutil.rmtree(run, ignore_errors=True)
os.makedirs(run)
version = lib.latest_template("riscv64", guest)
overlay, code_fd, vars_fd = lib.prepare_run_dir(run, "riscv64", guest, version)
real = backends.qemu_vm_argv
def argv(*a, **k):
    v = real(*a, **k)
    v[v.index("-cpu") + 1] = cpu
    if plugin:
        v += ["-plugin", f"{plugin},out={run}/trace.txt"]
    return v
backends.qemu_vm_argv = argv
b = backends.QemuBackend(run, "riscv64", smp, os.environ.get("PIN", "24-27"), overlay, code_fd, vars_fd, log)
s = b.launch()
res = {"guest": guest, "cpu": cpu, "smp": smp, "plugin": bool(plugin)}
try:
    b.release_cpus()
    agent, hello = lib.wait_for_agent(b.agent_socket(), float(os.environ.get("DEADLINE", "900")), vm_exited=s.exit_code)
    res["boot_to_agent_ready"] = round(s.elapsed(), 2)
    res["hello"] = {k: hello.get(k) for k in ("agent_version", "os")}
    agent.close()
except Exception as e:
    res["error"] = repr(e)
finally:
    try:
        import socket
        q = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); q.settimeout(30)
        q.connect(b.qmp_sock); f = q.makefile("rw")
        f.readline(); f.write('{"execute":"qmp_capabilities"}\n'); f.flush(); f.readline()
        f.write('{"execute":"quit"}\n'); f.flush()
        s.wait_exit(60)
        if s.proc.poll() is None: s.kill()
    except Exception as e:
        res["quit_error"] = repr(e)
        s.kill()
for f in ("overlay.qcow2", "vars.fd"):
    try: os.remove(os.path.join(run, f))
    except OSError: pass
print("RESULT", json.dumps(res), flush=True)
