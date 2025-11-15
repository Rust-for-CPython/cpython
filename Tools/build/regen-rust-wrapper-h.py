import os
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WRAPPER_H = ROOT / "Modules" / "cpython-sys" / "wrapper.h"
SKIP_PREFIXES = ("cpython/",)
SKIP_EXACT = {
    "internal/pycore_crossinterp_data_registry.h",
}

def normalize_path(header: str) -> str:
    return re.sub(r'(:?\.\/)(:?Include\/)?', '', header)

def main(output: str = WRAPPER_H) -> None:
    headers = os.environ.get("PYTHON_HEADERS")
    if headers is None:
        raise RuntimeError("Unable to read $PYTHON_HEADERS!")
    with open(output, "w") as f:
        f.write("#define Py_BUILD_CORE\n")
        f.write("#include \"Modules/expat/expat.h\"\n")
        for header in headers.split():
            normalized_path = normalize_path(header)
            if normalized_path.startswith(SKIP_PREFIXES):
                continue
            if normalized_path in SKIP_EXACT:
                continue
            f.write(f"#include \"{normalized_path}\"\n")
            if normalized_path == "Python/remote_debug.h":
                f.write("#undef UNUSED\n")

if __name__ == "__main__":
    import sys
    main(*sys.argv[1:])
