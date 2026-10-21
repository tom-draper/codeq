"""Source traversal shared by evaluation checks and generic tools."""
import os
from pathlib import Path


ARTIFACT_DIRS = {".git", ".codeq", "__pycache__"}


def source_paths(root):
    root = Path(root)
    for directory, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in ARTIFACT_DIRS)
        for name in list(dirs):
            path = Path(directory) / name
            if path.is_symlink():
                dirs.remove(name)
                yield path
        for name in sorted(files):
            yield Path(directory) / name
