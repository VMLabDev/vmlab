#!/usr/bin/env python3
"""Cross-compile the x64 UMDF driver using hash-locked Microsoft packages."""

import argparse
import configparser
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile

SOURCE = Path(__file__).resolve().parent
ROOT = SOURCE.parent.parent


def run(*command):
    print("+", " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), check=True)


def fetch(cache):
    dependencies = json.loads((SOURCE / "dependencies.json").read_text())
    dependencies.update(json.loads((SOURCE / "build-vm/toolchain/toolchain.json").read_text())["packages"])
    directories = {}
    for name, entry in dependencies.items():
        directory = cache / (name + "-" + entry["sha256"])
        archive = directory.with_suffix(".zip")
        if archive.exists():
            print("Using cached", name, archive, "(no download)", flush=True)
        else:
            print("Downloading", name, "from", entry["url"], flush=True)
            with tempfile.NamedTemporaryFile(dir=cache, delete=False) as output:
                with urllib.request.urlopen(entry["url"], timeout=120) as response:
                    shutil.copyfileobj(response, output)
                temporary = Path(output.name)
            temporary.replace(archive)
        if hashlib.file_digest(archive.open("rb"), "sha256").hexdigest() != entry["sha256"]:
            raise RuntimeError(f"SHA256 mismatch: {archive}")
        if not directory.exists():
            with zipfile.ZipFile(archive) as package:
                for item in package.infolist():
                    target = (directory / item.filename).resolve()
                    if not target.is_relative_to(directory.resolve()):
                        raise RuntimeError(f"Unsafe archive path: {item.filename}")
                package.extractall(directory)
        directories[name] = directory
    return directories


def overlay(directory):
    contents = []
    for path in sorted(directory.iterdir()):
        if path.is_dir():
            contents.append(overlay(path))
        else:
            contents.append({"type": "file", "name": path.name, "external-contents": str(path)})
    return {"type": "directory", "name": str(directory), "contents": contents}


def library(directory, name):
    matches = [path for path in directory.iterdir() if path.name.lower() == name.lower()]
    if len(matches) != 1:
        raise RuntimeError(f"Expected one {name} in {directory}, found {matches}")
    return matches[0]


def library_aliases(directory, output):
    output.mkdir(parents=True, exist_ok=True)
    for path in directory.iterdir():
        if path.suffix.lower() == ".lib":
            alias = output / path.name.lower()
            if not alias.exists():
                alias.symlink_to(path)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, default=ROOT / ".ci-cache/vdisplay-windows")
    parser.add_argument("--output", type=Path, default=SOURCE / "target/package")
    parser.add_argument("--fetch-only", action="store_true")
    parser.add_argument("--signing-tools", type=Path, help="stage the pinned Windows catalog and signing tools here")
    arguments = parser.parse_args()
    cache, output = arguments.cache.resolve(), arguments.output.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    packages = fetch(cache)
    if arguments.signing_tools:
        tools = arguments.signing_tools.resolve()
        shutil.copytree(packages["wdk"] / "c/bin/10.0.26100.0/x86", tools / "wdk", dirs_exist_ok=True)
        shutil.copytree(packages["sdk"] / "c/bin/10.0.26100.0/x64", tools / "sdk", dirs_exist_ok=True)
    if arguments.fetch_only:
        return
    output.mkdir(parents=True, exist_ok=True)
    work = output.parent / (output.name + "-build")
    work.mkdir(parents=True, exist_ok=True)
    compiler = shutil.which("clang-cl")
    compiler_command = [compiler] if compiler else [shutil.which("clang-18") or "clang", "--driver-mode=cl"]
    linker = shutil.which("lld-link") or shutil.which("lld-link-18")
    if not shutil.which(compiler_command[0]) or not linker:
        raise RuntimeError("clang-cl and lld-link are required (Ubuntu: apt install clang lld)")
    sdk = packages["sdk"] / "c/Include/10.0.26100.0"
    wdk = packages["wdk"] / "c"
    crt_include = next(packages["crt-headers"].glob("Contents/VC/Tools/MSVC/*/include"))
    crt_lib = next(packages["crt-libs"].glob("Contents/VC/Tools/MSVC/*/lib/x64"))
    crt_aliases = next(packages["crt-aliases"].glob("Contents/VC/Tools/MSVC/*/lib/x64/store"))
    includes = [sdk / "um", sdk / "shared", sdk / "ucrt", crt_include,
                wdk / "Include/10.0.26100.0/um", wdk / "Include/10.0.26100.0/shared",
                wdk / "Include/wdf/umdf/2.27", wdk / "Include/10.0.26100.0/um/iddcx/1.4"]
    includes = [path for path in includes if path.exists()]
    vfs = work / "headers.json"
    vfs.write_text(json.dumps({"version": 0, "case-sensitive": False,
                               "roots": [overlay(path) for path in includes]}))
    obj = work / "driver.obj"
    run(*compiler_command, "--target=x86_64-pc-windows-msvc", "/c", "/std:c++17", "/O2", "/MT", "/GS",
        "/GR-", "/W4", "/WX", "/Brepro", "/DUMDF_DRIVER", "/D_WIN32_WINNT=0x0A00", "/D_WIN64",
        "/D_AMD64_", "/DAMD64", "/DIDDCX_VERSION_MAJOR=1", "/DIDDCX_VERSION_MINOR=4",
        "/DIDDCX_MINIMUM_VERSION_REQUIRED=4", "/DUMDF_VERSION_MAJOR=2", "/DUMDF_VERSION_MINOR=27",
        "/DUMDF_MINIMUM_VERSION_REQUIRED=27",
        "/clang:-ivfsoverlay", "/clang:" + str(vfs),
        *["/imsvc" + str(path) for path in includes], SOURCE / "driver.cpp", "/Fo" + str(obj))
    sdk_lib = packages["sdk-libs"] / "c/um/x64"
    ucrt_lib = packages["sdk-libs"] / "c/ucrt/x64"
    crt_lib = library_aliases(crt_lib, work / "lib/crt")
    library_aliases(crt_aliases, work / "lib/crt")
    ucrt_lib = library_aliases(ucrt_lib, work / "lib/ucrt")
    libraries = [library(sdk_lib, name + ".lib") for name in
                 ["kernel32", "ntdll", "cfgmgr32", "d3d11", "dxgi", "dxguid", "avrt", "uuid", "advapi32"]]
    run(linker, "/dll", "/machine:x64", "/subsystem:windows,10.0", "/Brepro", "/dynamicbase", "/nxcompat",
        "/out:" + str(output / "vmlab-display.dll"), "/implib:" + str(work / "vmlab-display.lib"),
        "/libpath:" + str(crt_lib), "/libpath:" + str(ucrt_lib),
        obj, *libraries, wdk / "Lib/wdf/umdf/x64/2.27/WdfDriverStubUm.lib",
        wdk / "Lib/10.0.26100.0/um/x64/iddcx/1.4/iddcxstub.lib")
    run(*compiler_command, "--target=x86_64-pc-windows-msvc", "/c", "/std:c++17", "/O2", "/MT", "/GS",
        "/W4", "/WX", "/Brepro", "/D_WIN32_WINNT=0x0A00", "/clang:-ivfsoverlay", "/clang:" + str(vfs),
        *["/imsvc" + str(path) for path in includes], SOURCE / "install.cpp", "/Fo" + str(work / "install.obj"))
    run(linker, "/machine:x64", "/subsystem:console,10.0", "/Brepro", "/dynamicbase", "/nxcompat",
        "/out:" + str(output / "install.exe"), "/libpath:" + str(crt_lib), "/libpath:" + str(ucrt_lib),
        work / "install.obj", *[library(sdk_lib, name + ".lib") for name in ["kernel32", "setupapi", "newdev", "uuid", "user32", "advapi32"]])
    for name in ["vmlab-display.inf", "sign.ps1", "install.ps1", "LICENSE-Microsoft.txt", "LICENSE-IVSHMEM.txt"]:
        shutil.copyfile(SOURCE / name, output / name)
    (output / "ivshmem").mkdir(exist_ok=True)
    for name in ["ivshmem.inf", "ivshmem.cat", "ivshmem.sys"]:
        shutil.copyfile(packages["ivshmem"] / "Win10/amd64" / name, output / "ivshmem" / name)
    source_files = ["driver.cpp", "protocol.h", "ivshmem.h", "install.cpp", "vmlab-display.inf",
                    "build.py", "build.ps1", "sign.ps1", "install.ps1", "dependencies.json", "build-vm/toolchain/toolchain.json"]
    sources = {name: hashlib.sha256((SOURCE / name).read_bytes()).hexdigest() for name in source_files}
    identity = hashlib.sha256("".join(f"{name}={sources[name]}\n" for name in sorted(sources)).encode()).hexdigest()
    inf = configparser.ConfigParser(interpolation=None, allow_no_value=True)
    inf.optionxform = str
    inf.read(output / "vmlab-display.inf")
    inf["Version"]["DriverVer"] = f"11/04/2026,1.0.{int(identity[:4], 16)}.{int(identity[4:8], 16)}"
    with (output / "vmlab-display.inf").open("w") as file:
        inf.write(file, space_around_delimiters=False)
    (output / "BUILDINFO.json").write_text(json.dumps({"sources": sources,
        "compiler": subprocess.check_output([compiler_command[0], "--version"], text=True).splitlines()[0]}, indent=2) + "\n")
    print("Compiled:", output / "vmlab-display.dll", flush=True)


if __name__ == "__main__":
    main()
