"""The flat aarch64 images on our interpreter loop and on QEMU TCG (card C5).

Two workloads, both without an MMU, a device or a system register: the
scalar loop of `bench/a64/loop.S` and the CoreMark algorithm mix of
`bench/a64/coremark.c`, checked against CoreMark's own checksums. An image
ends with the semihosting call SYS_EXIT. Our interpreter
(`vmlab-hv-tcg-aarch64`, example `a64_bench`) stops in front of its `hlt`,
and QEMU ends with the exit code of the call, so both run the same bytes to
the same instruction.

The `interpreter` backend records the time of the run inside the process,
with the translation of the blocks in it. The `qemu` backend (guest
`flat-a64`) records the wall time of a QEMU process less the median of a
startup baseline (the loop image with one iteration). MIPS on both sides
uses the instruction count of our interpreter on the same image. The
history line holds the seconds as `samples`, as every other line does, and
the MIPS as `mips_samples` and `mips_median`.

QEMU is the yardstick here and nothing else: no product path starts it."""
import datetime
import json
import os
import statistics
import subprocess
from pathlib import Path

import lib
import micro

GUEST = "flat-a64"
LOOP_ITERATIONS = 100_000_000
STARTUP_ITERATIONS = 1
COREMARK_ITERATIONS = 1000
# CoreMark's own checksums for a performance run (seeds 0, 0, 0x66) on 2000
# bytes: list, matrix, state. The image leaves them in X2 to X4 and its
# error count in X5.
COREMARK_CRCS = {"x2": 0xE714, "x3": 0x1FD7, "x4": 0x8E3A, "x5": 0}
# The most instructions a workload may run before it counts as hung.
INSTRUCTION_LIMIT = 4_000_000_000

QEMU_BINARY = "qemu-system-aarch64"
# The machine and the CPU of the lockstep oracle (DESIGN.md O3). The image
# is the ELF, loaded at its own addresses and entered at EL1.
QEMU_MACHINE_ARGS = ("-machine", "virt", "-cpu", "cortex-a53", "-accel", "tcg",
                     "-smp", "1", "-m", "128M",
                     "-semihosting-config", "enable=on,target=native")
QEMU_MEMORY_MAX = "8G"
WORKLOADS = (f"scalar_loop_{3 * LOOP_ITERATIONS}", f"coremark_algorithms_{COREMARK_ITERATIONS}")


def a64_flags():
    """The flags every flat aarch64 image builds with: no FP or vector
    register (the vector lifters and CPACR_EL1 come with later cards), and
    aligned accesses only (the MMU is off)."""
    return ["clang", "--target=aarch64-unknown-elf", "-march=armv8-a",
            "-mgeneral-regs-only", "-mstrict-align", "-O2", "-ffreestanding",
            "-fno-builtin", "-fno-pic", "-ffunction-sections", "-fdata-sections"]


def link(objects, elf):
    """Link without relaxation: lld turns an `adrp` and `add` pair into a
    `nop` and an `adr`, and the hint space has no lifter before card C6."""
    port = Path(lib.REPO_DIR) / "bench/a64"
    subprocess.run([*a64_flags(), "-nostdlib", "-static", "-fuse-ld=lld", "-Wl,--gc-sections",
                    "-Wl,--build-id=none", "-Wl,--no-relax", "-T", str(port / "link.ld"), *objects, "-o", str(elf)],
                   check=True)
    image = elf.with_suffix(".bin")
    subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), str(image)], check=True)
    return elf, image


def output_dir():
    output = Path(lib.REPO_DIR) / "target/tcg/micro-a64"
    output.mkdir(parents=True, exist_ok=True)
    return output


def build_loop(iterations):
    """bench/a64/loop.S as an ELF (for QEMU) and as a flat image."""
    if not 1 <= iterations < 1 << 32:
        raise lib.BenchError("loop iterations must be 1 to 2^32 - 1")
    port = Path(lib.REPO_DIR) / "bench/a64"
    return link([f"-DLOOP_ITERATIONS={iterations}", str(port / "loop.S")],
                output_dir() / f"loop-{iterations}.elf")


def loop_expected_instructions(iterations):
    """3 * N of the loop, the five of the stub, and the nine of the exit up
    to its `hlt` (bench/a64/loop.S)."""
    return 3 * iterations + 14


def build_coremark(iterations):
    if iterations < 1:
        raise lib.BenchError("CoreMark iterations must be positive")
    source = micro.coremark_source()
    port = Path(lib.REPO_DIR) / "bench/a64"
    output = output_dir()
    flags = [*a64_flags(), "-I" + str(port), "-I" + str(source), f"-DITERATIONS={iterations}"]
    objects = []
    for name in ("core_main", "core_list_join", "core_matrix", "core_state", "core_util"):
        dest = output / f"{name}-{iterations}.o"
        rename = ["-Dmain=unused_coremark_main"] if name == "core_main" else []
        subprocess.run([*flags, *rename, "-c", str(source / (name + ".c")), "-o", str(dest)],
                       check=True)
        objects.append(str(dest))
    return link([f"-I{port}", f"-I{source}", f"-DITERATIONS={iterations}",
                 str(port / "start.S"), str(port / "coremark.c"), *objects],
                output / f"coremark-{iterations}.elf")


def qemu_version():
    return subprocess.check_output([QEMU_BINARY, "--version"], text=True).splitlines()[0].strip()


def qemu_argv(kernel):
    return [QEMU_BINARY, *QEMU_MACHINE_ARGS, "-kernel", str(kernel),
            "-display", "none", "-serial", "none", "-monitor", "none"]


def check_guest(name, result):
    """The guest's own verdict: its exit code, and for CoreMark the three
    checksums that the image left in its registers."""
    if result.get("exit_code") != 0:
        raise lib.BenchError(f"{name}: the guest failed: {result}")
    if name.startswith("coremark"):
        got = {key: result.get(key) for key in COREMARK_CRCS}
        if got != COREMARK_CRCS:
            raise lib.BenchError(f"{name}: CoreMark checksums {got} are not {COREMARK_CRCS}")


def build_workloads():
    """name -> (elf, image, the instruction count that is known ahead)."""
    loop_elf, loop_image = build_loop(LOOP_ITERATIONS)
    coremark_elf, coremark_image = build_coremark(COREMARK_ITERATIONS)
    micro.elf_loads_image(loop_elf, loop_image)
    micro.elf_loads_image(coremark_elf, coremark_image)
    return {WORKLOADS[0]: (loop_elf, loop_image, loop_expected_instructions(LOOP_ITERATIONS)),
            WORKLOADS[1]: (coremark_elf, coremark_image, None)}


def interpreter_binary():
    subprocess.run(["cargo", "build", "--release", "-p", "vmlab-hv-tcg-aarch64",
                    "--example", "a64_bench"], cwd=lib.REPO_DIR, check=True)
    # The target directory is where cargo says it is: a build host can
    # move it out of the checkout (CARGO_TARGET_DIR, a cargo config).
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=lib.REPO_DIR, text=True))
    return str(Path(metadata["target_directory"]) / "release/examples/a64_bench")


def run_interpreter(binary, image, pin, environment):
    return micro.measure([*lib.systemd_run_prefix(), *lib.taskset_prefix(pin), binary,
                          str(image), str(INSTRUCTION_LIMIT)], environment)


def record(backend, name, commit, dirty, host, samples, mips, noise, raw):
    return {"recorded": datetime.datetime.now().isoformat(timespec="seconds"),
            "commit": commit, "dirty": dirty, "host": host, "backend": backend,
            "arch": "aarch64", "guest": GUEST, "vcpus": 1, "workload": name,
            "unit": "s", "direction": "lower", "repeats": len(samples), "samples": samples,
            "median": statistics.median(samples), "spread": max(samples) - min(samples),
            "mips_samples": mips, "mips_median": statistics.median(mips),
            "qemu_baseline": None, "env": noise, "measurements": raw}


def noise_record(pin, memory_max, backend_version):
    return {"governor": lib.read_governor(), "loadavg_start": lib.read_loadavg(),
            "pin": pin, "memory_max": memory_max, "host_cpus": os.cpu_count(),
            "coremark_revision": micro.COREMARK_REVISION, "backend_version": backend_version,
            "warmup_excluded": False}


def run_qemu(args, commit, dirty, host, log, pin):
    binary = interpreter_binary()
    workloads = build_workloads()
    startup_elf, _ = build_loop(STARTUP_ITERATIONS)
    environment = dict(os.environ)
    prefix = [*lib.systemd_run_prefix(QEMU_MEMORY_MAX), *lib.taskset_prefix(pin)]
    # One run that is not recorded: the recorded ones then miss the cold
    # page cache of the QEMU binary, which is not guest execution.
    micro.measure_qemu([*prefix, *qemu_argv(startup_elf)], environment)
    startup_samples = [micro.measure_qemu([*prefix, *qemu_argv(startup_elf)], environment)[0]
                       for _ in range(max(args.repeats, micro.FLAT_QEMU_MIN_REPEATS))]
    startup = statistics.median(startup_samples)
    log(f"qemu startup baseline: median {startup:.6f} s over {len(startup_samples)} repeats")
    for name, (elf, image, expected) in workloads.items():
        counted = run_interpreter(binary, image, pin, environment)
        check_guest(name, counted)
        instructions = counted["instructions"]
        if expected is not None and instructions != expected:
            raise lib.BenchError(f"{name}: interpreter count {instructions} != {expected}")
        noise = noise_record(pin, QEMU_MEMORY_MAX, qemu_version())
        noise.update({"qemu": {"machine": "virt", "cpu": "cortex-a53", "accel": "tcg",
                               "memory": "128M", "kernel": "elf with semihosting SYS_EXIT"},
                      "startup_samples": startup_samples, "startup_median": startup,
                      "instruction_count": instructions,
                      "instruction_count_source": "interpreter run of the same flat image"})
        samples, mips, raw = [], [], []
        for repeat in range(args.repeats):
            # QEMU ends with the exit code of the guest: a wrong checksum
            # fails the run in measure_qemu.
            wall, _, _ = micro.measure_qemu([*prefix, *qemu_argv(elf)], environment)
            net = wall - startup
            if net <= 0:
                raise lib.BenchError(f"{name}: wall {wall:.6f} s is not above the startup"
                                     f" median {startup:.6f} s")
            samples.append(net)
            mips.append(instructions / net / 1e6)
            raw.append({"wall_seconds": wall, "net_seconds": net, "instructions": instructions,
                        "mips": mips[-1]})
            log(f"{name} repeat {repeat + 1}: {mips[-1]:.2f} MIPS, {net:.6f} s net"
                f" ({wall:.6f} s wall)")
        noise["loadavg_end"] = lib.read_loadavg()
        lib.append_history("aarch64", record("qemu", name, commit, dirty, host, samples, mips,
                                             noise, raw))
    return 0


def run_interpreter_backend(args, commit, dirty, host, log, pin):
    binary = interpreter_binary()
    workloads = build_workloads()
    environment = dict(os.environ)
    for name, (_, image, expected) in workloads.items():
        noise = noise_record(pin, lib.MEMORY_MAX, "packed block interpreter")
        samples, mips, raw = [], [], []
        for repeat in range(args.repeats):
            result = run_interpreter(binary, image, pin, environment)
            check_guest(name, result)
            if expected is not None and result["instructions"] != expected:
                raise lib.BenchError(f"{name}: count {result['instructions']} != {expected}")
            raw.append(result)
            samples.append(result["seconds"])
            mips.append(result["mips"])
            log(f"{name} repeat {repeat + 1}: {result['mips']:.2f} MIPS,"
                f" {result['instructions']} instructions")
        if len({r["instructions"] for r in raw}) != 1:
            raise lib.BenchError(f"nondeterministic instruction count: {name}")
        noise["instruction_count"] = raw[0]["instructions"]
        noise["loadavg_end"] = lib.read_loadavg()
        rec = record("interpreter", name, commit, dirty, host, samples, mips, noise, raw)
        rec["qemu_baseline"] = lib.latest_baseline(lib.read_history("aarch64"), "aarch64",
                                                   GUEST, 1, name)
        lib.append_history("aarch64", rec)
    return 0


def run(args, commit, dirty, host, log):
    if (args.vcpus or "1") != "1":
        raise lib.BenchError("the flat aarch64 images run on one CPU: --vcpus 1")
    if args.repeats < 1:
        raise lib.BenchError("repeats must be positive")
    pin = lib.pin_cpus(1, args.cpus)
    log(f"flat aarch64 micro-benchmarks: {args.backend}, pin={pin}")
    if args.dry_run:
        return 0
    if args.backend == "qemu":
        return run_qemu(args, commit, dirty, host, log, pin)
    return run_interpreter_backend(args, commit, dirty, host, log, pin)
