import os
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INCLUDE = ROOT / "Include"
WRAPPER_H = ROOT / "Modules" / "cpython-sys" / "wrapper.h"

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
            f.write(f"#include \"{normalized_path}\"\n")

if __name__ == "__main__":
    import sys
    main(*sys.argv[1:])
