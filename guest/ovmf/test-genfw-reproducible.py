#!/usr/bin/env python3
"""Check that GenFw's CodeView record does not depend on the build path."""

import argparse
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("genfw", type=Path)
    args = parser.parse_args()
    genfw = args.genfw.resolve()
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        outputs = []
        for name in ("build.aaaaaa", "a-different-checkout/build.bbbbbb"):
            directory = root / name
            directory.mkdir(parents=True)
            source = directory / "entry.c"
            source.write_text("void _start(void) {}\nconst char *source_path(void) { return __FILE__; }\n")
            image = directory / "module.dll"
            subprocess.run([
                "gcc", "-nostdlib", "-shared", "-Wl,-Bsymbolic", "-Wl,--build-id=none",
                "-Wl,-e,_start", "-fno-asynchronous-unwind-tables",
                f"-ffile-prefix-map={directory}/=", "-o", str(image), str(source),
            ], check=True)
            output = directory / "module.efi"
            subprocess.run([str(genfw), "-e", "DXE_DRIVER", "-o", str(output), str(image)], check=True)
            outputs.append(output.read_bytes())
        assert outputs[0] == outputs[1], "GenFw output depends on the absolute build path"
        assert b"module.dll\0" in outputs[0], "the CodeView module name was lost"
        assert b"entry.c\0" in outputs[0], "the source filename was lost"
        assert str(root).encode() not in outputs[0], "the temporary build path remains in CodeView"
        print("GenFw emits identical PE images from different absolute build paths")


if __name__ == "__main__":
    main()
